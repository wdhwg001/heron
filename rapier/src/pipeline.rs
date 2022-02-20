use std::marker::PhantomData;

use bevy::app::Events;
use bevy::ecs::prelude::*;
use bevy::ecs::system::SystemParam;
use bevy::log::prelude::*;
use bevy::math::Quat;
use bevy::math::Vec2;
use bevy::math::Vec3;
use crossbeam::channel::{Receiver, Sender};

use heron_core::{
    CollisionData, CollisionEvent, CollisionLayers, CollisionShape, Gravity, PhysicsStepDuration,
    PhysicsSteps, PhysicsTime,
};
pub use physics_world::PhysicsWorld;

use crate::convert::{IntoBevy, IntoRapier};
use crate::rapier::dynamics::{
    CCDSolver, IntegrationParameters, IslandManager, JointSet, RigidBodySet,
};
use crate::rapier::geometry::{
    BroadPhase, ColliderHandle, ColliderSet, ContactEvent, InteractionGroups, IntersectionEvent,
    NarrowPhase,
};
use crate::rapier::parry::query::{Ray, TOIStatus};
use crate::rapier::pipeline::{EventHandler, PhysicsPipeline, QueryPipeline};
use crate::shape::ColliderFactory;

// We have to make a module here so that we can allow missing docs on the structs generated by the
// derive macro
#[allow(missing_docs)]
mod physics_world {
    #[allow(clippy::wildcard_imports)]
    // Fine right here because this module is a workaround anyway
    use super::*;

    /// A Bevy system parameter that can be used to perform queries such as ray casts on the physics
    /// world
    ///
    /// See the [`ray_casting`](https://github.com/jcornaz/heron/blob/main/examples/ray_casting.rs)
    /// example for a detailed usage example.
    #[derive(SystemParam)]
    pub struct PhysicsWorld<'w, 's> {
        query_pipeline: ResMut<'w, QueryPipeline>,
        colliders: ResMut<'w, ColliderSet>,
        #[system_param(ignore)]
        marker: PhantomData<&'s usize>,
    }

    impl<'w, 's> PhysicsWorld<'w, 's> {
        /// Cast a ray and get the collision shape entity, point, and normal at which it collided,
        /// if any
        ///
        /// - `from`: The point to cast the ray from.
        /// - `ray`: A vector indicating the direction and the distance to cast the ray. If
        /// - `solid`: If `true` a point cast from the inside of a solid object will stop
        ///   immediately and the collision point will be the same as the `from` point. If `false` a
        ///   ray cast from inside of an object will act like the object is hollow and will hit the
        ///   surface of the object after traveling through the object interior.
        #[must_use]
        pub fn ray_cast(&self, start: Vec3, ray: Vec3, solid: bool) -> Option<RayCastInfo> {
            self.ray_cast_internal(start, ray, solid, CollisionLayers::default(), None)
        }

        /// Cast a ray with extra filters
        ///
        /// Behaves the same as [`ray_cast`](Self::ray_cast) but takes extra arguments for
        /// filtering results:
        ///
        /// - `layers`: The [`CollisionLayers`] to considered for collisions, allowing for coarse
        ///   filtering of collisions.
        /// - `filter`: A closure taking an [`Entity`] and returning `true` if the entity should be
        ///   considered for collisions, allowing for fine-grained, per-entity filtering of
        ///   collisions.
        #[must_use]
        pub fn ray_cast_with_filter<F>(
            &self,
            start: Vec3,
            ray: Vec3,
            solid: bool,
            layers: CollisionLayers,
            filter: F,
        ) -> Option<RayCastInfo>
        where
            F: Fn(Entity) -> bool,
        {
            self.ray_cast_internal(start, ray, solid, layers, Some(&filter))
        }

        /// Non-public implementation of `ray_cast`
        #[must_use]
        #[allow(clippy::cast_possible_truncation)]
        fn ray_cast_internal(
            &self,
            start: Vec3,
            ray: Vec3,
            solid: bool,
            layers: CollisionLayers,
            filter: Option<&dyn Fn(Entity) -> bool>,
        ) -> Option<RayCastInfo> {
            let direction = ray.try_normalize()?;
            let rapier_ray = Ray::new(start.into_rapier(), direction.into_rapier());

            let result = self.query_pipeline.cast_ray_and_get_normal(
                &*self.colliders,
                &rapier_ray,
                ray.length(),
                solid,
                InteractionGroups {
                    memberships: layers.groups_bits(),
                    filter: layers.masks_bits(),
                },
                // Map filter to one that takes a collider handle and returns a bool
                filter
                    .map(|filter| {
                        move |handle: ColliderHandle| -> bool {
                            self.colliders
                                .get(handle)
                                .map(|collider| Entity::from_bits(collider.user_data as u64))
                                .map_or(false, filter)
                        }
                    })
                    .as_ref()
                    .map(|x| x as &dyn Fn(ColliderHandle) -> bool),
            );

            result.map(|(collider_handle, intersection)| {
                Some(RayCastInfo {
                    collision_point: start + direction * intersection.toi,
                    entity: self
                        .colliders
                        .get(collider_handle)
                        .map(|collider| Entity::from_bits(collider.user_data as u64))?,
                    normal: intersection.normal.into_bevy(),
                })
            })?
        }

        /// Cast a shape and get the collision shape entity, point, and normal at which it collided, if
        /// any
        ///
        /// - `shape`: The [`CollisionShape`] to use for the shape cast
        /// - `start_position`: The position to start the shape cast at
        /// - `rotation`: The rotation of the collision shape
        /// - `end_posiion`: The end position of the shape cast
        ///
        /// # Panics
        ///
        /// This will panic if the start position and end position are the same.
        #[must_use]
        pub fn shape_cast(
            &self,
            shape: &CollisionShape,
            start_position: Vec3,
            start_rotation: Quat,
            ray: Vec3,
        ) -> Option<ShapeCastInfo> {
            self.shape_cast_internal(
                shape,
                start_position,
                start_rotation,
                ray,
                CollisionLayers::default(),
                None,
            )
        }

        /// Cast a shape with an optional filter
        ///
        /// Behaves the same as [`shape_cast`](Self::shape_cast) but takes extra arguments for
        /// filtering results:
        ///
        /// - `layers`: The [`CollisionLayers`] to considered for collisions, allowing for coarse
        ///   filtering of collisions.
        /// - `filter`: A closure taking an [`Entity`] and returning `true` if the entity should be
        ///   considered for collisions, allowing for fine-grained, per-entity filtering of
        ///   collisions.
        ///
        /// # Panics
        ///
        /// This will panic if the `from` point and the `to` point are the same.
        pub fn shape_cast_with_filter<F>(
            &self,
            shape: &CollisionShape,
            start_position: Vec3,
            start_rotation: Quat,
            ray: Vec3,
            layers: CollisionLayers,
            filter: F,
        ) -> Option<ShapeCastInfo>
        where
            F: Fn(Entity) -> bool,
        {
            self.shape_cast_internal(
                shape,
                start_position,
                start_rotation,
                ray,
                layers,
                Some(&filter),
            )
        }

        #[must_use]
        #[allow(clippy::cast_possible_truncation)]
        fn shape_cast_internal(
            &self,
            shape: &CollisionShape,
            start_position: Vec3,
            start_rotation: Quat,
            ray: Vec3,
            layers: CollisionLayers,
            filter: Option<&dyn Fn(Entity) -> bool>,
        ) -> Option<ShapeCastInfo> {
            let direction = ray.try_normalize()?;
            let collider = shape.collider_builder().build();

            let result = self.query_pipeline.cast_shape(
                &*self.colliders,
                &(start_position, start_rotation).into_rapier(),
                &direction.into_rapier(),
                collider.shape(),
                ray.length(),
                InteractionGroups {
                    memberships: layers.groups_bits(),
                    filter: layers.masks_bits(),
                },
                // Map filter to one that takes a collider handle and returns a bool
                filter
                    .map(|filter| {
                        move |handle: ColliderHandle| -> bool {
                            self.colliders
                                .get(handle)
                                .map(|collider| Entity::from_bits(collider.user_data as u64))
                                .map_or(false, filter)
                        }
                    })
                    .as_ref()
                    .map(|x| x as &dyn Fn(ColliderHandle) -> bool),
            );

            result.map(|(collider_handle, toi)| {
                let collision_type = match toi.status {
                    TOIStatus::OutOfIterations | TOIStatus::Converged | TOIStatus::Failed => {
                        // Get the position of the shape at the point of contact
                        let self_end_position = start_position + direction * toi.toi;

                        let self_point = toi.witness1.into_bevy();
                        #[cfg(dim2)]
                        let self_point = self_point.extend(0.);

                        let self_normal = toi.normal1.into_bevy();

                        let other_point = toi.witness2.into_bevy();
                        #[cfg(dim2)]
                        let other_point = other_point.extend(0.);

                        let other_normal = toi.normal2.into_bevy();

                        ShapeCastCollisionType::Collided(ShapeCastCollisionInfo {
                            self_end_position,
                            self_point,
                            self_normal,
                            other_point,
                            other_normal,
                        })
                    }
                    // If the shapes were already penetrating each-other, then the contact points are
                    // not going to be accurate
                    TOIStatus::Penetrating => ShapeCastCollisionType::AlreadyPenetrating,
                };

                Some(ShapeCastInfo {
                    entity: self
                        .colliders
                        .get(collider_handle)
                        .map(|collider| Entity::from_bits(collider.user_data as u64))?,
                    collision_type,
                })
            })?
        }
    }
}

/// The result of a [`PhysicsWorld::ray_cast`] operation
#[derive(Clone, Debug)]
pub struct RayCastInfo {
    /// The Point in the world that the ray collided with
    pub collision_point: Vec3,
    /// The collision shape entity that the ray collided with
    pub entity: Entity,
    /// The surface normal at the point of ray collision
    pub normal: Vec3,
}

/// The result of a [`PhysicsWorld::shape_cast`] operation
#[derive(Clone, Debug)]
pub struct ShapeCastInfo {
    /// The collision shape entity that the shape collided with
    pub entity: Entity,
    /// The information about the shape collision
    pub collision_type: ShapeCastCollisionType,
}

/// The type of collision returned from a shape cast
#[derive(Clone, Debug)]
pub enum ShapeCastCollisionType {
    /// The shapes were already penetrating each-other at the shapes `start_position`
    ///
    /// Collision normals and points cannot be accurately calculated
    AlreadyPenetrating,
    /// The cast shape collided with another along its path
    Collided(ShapeCastCollisionInfo),
}

/// Information about a shape cast collision
#[derive(Clone, Debug)]
pub struct ShapeCastCollisionInfo {
    /// The position of the cast shape when it collided
    pub self_end_position: Vec3,
    /// The collision point on the cast shape
    pub self_point: Vec3,
    /// The collision normal on the cast shape
    pub self_normal: Vec3,
    /// The collision point on the shape the cast collided with
    pub other_point: Vec3,
    /// The collision normal on the shape the cast collided with
    pub other_normal: Vec3,
}

pub(crate) fn update_integration_parameters(
    physics_steps: Res<'_, PhysicsSteps>,
    physics_time: Res<'_, PhysicsTime>,
    bevy_time: Res<'_, bevy::core::Time>,
    mut integration_parameters: ResMut<'_, IntegrationParameters>,
) {
    if matches!(
        physics_steps.duration(),
        PhysicsStepDuration::MaxDeltaTime(_)
    ) || physics_steps.is_changed()
        || physics_time.is_changed()
    {
        integration_parameters.dt = physics_steps
            .duration()
            .exact(bevy_time.delta())
            .as_secs_f32()
            * physics_time.scale();
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn step(
    mut pipeline: ResMut<'_, PhysicsPipeline>,
    mut query_pipeline: ResMut<'_, QueryPipeline>,
    gravity: Res<'_, Gravity>,
    integration_parameters: Res<'_, IntegrationParameters>,
    mut islands: ResMut<'_, IslandManager>,
    mut broad_phase: ResMut<'_, BroadPhase>,
    mut narrow_phase: ResMut<'_, NarrowPhase>,
    mut bodies: ResMut<'_, RigidBodySet>,
    mut colliders: ResMut<'_, ColliderSet>,
    mut joints: ResMut<'_, JointSet>,
    mut ccd_solver: ResMut<'_, CCDSolver>,
    event_manager: Local<'_, EventManager>,
    mut events: ResMut<'_, Events<CollisionEvent>>,
) {
    let gravity = Vec3::from(*gravity).into_rapier();

    // Step the physics simulation
    pipeline.step(
        &gravity,
        &integration_parameters,
        &mut islands,
        &mut broad_phase,
        &mut narrow_phase,
        &mut bodies,
        &mut colliders,
        &mut joints,
        &mut ccd_solver,
        &(),
        &*event_manager,
    );

    // Update the query pipleine
    query_pipeline.update(&islands, &bodies, &colliders);

    event_manager.fire_events(&narrow_phase, &bodies, &colliders, &mut events);
}

pub(crate) struct EventManager {
    contact_recv: Receiver<ContactEvent>,
    intersection_recv: Receiver<IntersectionEvent>,
    contact_send: Sender<ContactEvent>,
    intersection_send: Sender<IntersectionEvent>,
}

impl EventHandler for EventManager {
    fn handle_intersection_event(&self, event: IntersectionEvent) {
        if self.intersection_send.send(event).is_err() {
            error!("Failed to forward intersection event!");
        }
    }

    fn handle_contact_event(&self, event: ContactEvent, _: &crate::rapier::prelude::ContactPair) {
        if self.contact_send.send(event).is_err() {
            error!("Failed to forward contact event!");
        }
    }
}

impl Default for EventManager {
    fn default() -> Self {
        let (contact_send, contact_recv) = crossbeam::channel::unbounded();
        let (intersection_send, intersection_recv) = crossbeam::channel::unbounded();
        Self {
            contact_recv,
            intersection_recv,
            contact_send,
            intersection_send,
        }
    }
}

impl EventManager {
    fn fire_events(
        &self,
        narrow_phase: &NarrowPhase,
        bodies: &RigidBodySet,
        colliders: &ColliderSet,
        events: &mut Events<CollisionEvent>,
    ) {
        while let Ok(event) = self.contact_recv.try_recv() {
            match event {
                ContactEvent::Started(h1, h2) => {
                    if let Some((d1, d2)) = Self::data(narrow_phase, bodies, colliders, h1, h2) {
                        events.send(CollisionEvent::Started(d1, d2));
                    }
                }
                ContactEvent::Stopped(h1, h2) => {
                    if let Some((d1, d2)) = Self::data(narrow_phase, bodies, colliders, h1, h2) {
                        events.send(CollisionEvent::Stopped(d1, d2));
                    }
                }
            }
        }

        while let Ok(IntersectionEvent {
            collider1,
            collider2,
            intersecting,
        }) = self.intersection_recv.try_recv()
        {
            if let Some((e1, e2)) =
                Self::data(narrow_phase, bodies, colliders, collider1, collider2)
            {
                if intersecting {
                    events.send(CollisionEvent::Started(e1, e2));
                } else {
                    events.send(CollisionEvent::Stopped(e1, e2));
                }
            }
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    fn data(
        narrow_phase: &NarrowPhase,
        bodies: &RigidBodySet,
        colliders: &ColliderSet,
        h1: ColliderHandle,
        h2: ColliderHandle,
    ) -> Option<(CollisionData, CollisionData)> {
        if let (Some(collider1), Some(collider2)) = (colliders.get(h1), colliders.get(h2)) {
            if let (Some(rb1), Some(rb2)) = (
                collider1.parent().and_then(|parent| bodies.get(parent)),
                collider2.parent().and_then(|parent| bodies.get(parent)),
            ) {
                let normals1 = narrow_phase
                    .contact_pair(h1, h2)
                    .map(|contact_pair| {
                        contact_pair
                            .manifolds
                            .iter()
                            .map(|manifold| {
                                Vec2::new(manifold.data.normal.x, manifold.data.normal.y)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let normals2 = narrow_phase
                    .contact_pair(h2, h1)
                    .map(|contact_pair| {
                        contact_pair
                            .manifolds
                            .iter()
                            .map(|manifold| {
                                Vec2::new(manifold.data.normal.x, manifold.data.normal.y)
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                let d1 = CollisionData::new(
                    Entity::from_bits(rb1.user_data as u64),
                    Entity::from_bits(collider1.user_data as u64),
                    collider1.collision_groups().into_bevy(),
                    normals1,
                );
                let d2 = CollisionData::new(
                    Entity::from_bits(rb2.user_data as u64),
                    Entity::from_bits(collider2.user_data as u64),
                    collider2.collision_groups().into_bevy(),
                    normals2,
                );
                Some(
                    if Entity::from_bits(rb1.user_data as u64)
                        < Entity::from_bits(rb2.user_data as u64)
                    {
                        (d1, d2)
                    } else {
                        (d2, d1)
                    },
                )
            } else {
                None
            }
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use bevy::prelude::*;
    use bevy::MinimalPlugins;

    use heron_core::CollisionLayers;
    use heron_core::RigidBody;

    use crate::pipeline::EventManager;
    use crate::rapier::dynamics::RigidBodyBuilder;
    use crate::rapier::geometry::{ColliderBuilder, ColliderHandle};
    use crate::RapierPlugin;

    use super::*;

    struct TestContext {
        narrow_phase: NarrowPhase,
        bodies: RigidBodySet,
        colliders: ColliderSet,
        rb_entity_1: Entity,
        rb_entity_2: Entity,
        collider_entity_1: Entity,
        collider_entity_2: Entity,
        layers_1: CollisionLayers,
        layers_2: CollisionLayers,
        handle1: ColliderHandle,
        handle2: ColliderHandle,
    }

    impl Default for TestContext {
        fn default() -> Self {
            let narrow_phase = NarrowPhase::new();
            let mut bodies = RigidBodySet::new();
            let mut colliders = ColliderSet::new();

            let rb_entity_1 = Entity::from_bits(0);
            let rb_entity_2 = Entity::from_bits(1);
            let collider_entity_1 = Entity::from_bits(2);
            let collider_entity_2 = Entity::from_bits(3);
            let layers_1 = CollisionLayers::from_bits(1, 2);
            let layers_2 = CollisionLayers::from_bits(3, 4);
            let body1 = bodies.insert(
                RigidBodyBuilder::new_dynamic()
                    .user_data(rb_entity_1.to_bits().into())
                    .build(),
            );
            let body2 = bodies.insert(
                RigidBodyBuilder::new_dynamic()
                    .user_data(rb_entity_2.to_bits().into())
                    .build(),
            );
            let handle1 = colliders.insert_with_parent(
                ColliderBuilder::ball(1.0)
                    .user_data(collider_entity_1.to_bits().into())
                    .collision_groups(layers_1.into_rapier())
                    .build(),
                body1,
                &mut bodies,
            );
            let handle2 = colliders.insert_with_parent(
                ColliderBuilder::ball(1.0)
                    .user_data(collider_entity_2.to_bits().into())
                    .collision_groups(layers_2.into_rapier())
                    .build(),
                body2,
                &mut bodies,
            );

            Self {
                narrow_phase,
                bodies,
                colliders,
                rb_entity_1,
                rb_entity_2,
                collider_entity_1,
                collider_entity_2,
                layers_1,
                layers_2,
                handle1,
                handle2,
            }
        }
    }

    #[test]
    fn contact_started_fires_collision_started() {
        let manager = EventManager::default();
        let context = TestContext::default();

        manager
            .contact_send
            .send(ContactEvent::Started(context.handle1, context.handle2))
            .unwrap();

        let mut events = Events::<CollisionEvent>::default();
        manager.fire_events(
            &context.narrow_phase,
            &context.bodies,
            &context.colliders,
            &mut events,
        );
        let events: Vec<CollisionEvent> = events.get_reader().iter(&events).cloned().collect();

        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert!(matches!(event, CollisionEvent::Started(_, _)));
        assert_eq!(
            event.collision_shape_entities(),
            (context.collider_entity_1, context.collider_entity_2)
        );
    }

    #[test]
    fn contact_stopped_fires_collision_stopped() {
        let manager = EventManager::default();
        let context = TestContext::default();

        manager
            .contact_send
            .send(ContactEvent::Stopped(context.handle1, context.handle2))
            .unwrap();

        let mut events = Events::<CollisionEvent>::default();
        manager.fire_events(
            &context.narrow_phase,
            &context.bodies,
            &context.colliders,
            &mut events,
        );
        let events: Vec<CollisionEvent> = events.get_reader().iter(&events).cloned().collect();

        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert!(matches!(event, CollisionEvent::Stopped(_, _)));
        assert_eq!(
            event.collision_shape_entities(),
            (context.collider_entity_1, context.collider_entity_2)
        );
    }

    #[test]
    fn intersection_true_fires_collision_started() {
        let manager = EventManager::default();
        let context = TestContext::default();

        manager
            .intersection_send
            .send(IntersectionEvent::new(
                context.handle1,
                context.handle2,
                true,
            ))
            .unwrap();

        let mut events = Events::<CollisionEvent>::default();
        manager.fire_events(
            &context.narrow_phase,
            &context.bodies,
            &context.colliders,
            &mut events,
        );
        let events: Vec<CollisionEvent> = events.get_reader().iter(&events).cloned().collect();

        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert!(matches!(event, CollisionEvent::Started(_, _)));
        assert_eq!(
            event.collision_shape_entities(),
            (context.collider_entity_1, context.collider_entity_2)
        );
    }

    #[test]
    fn intersection_false_fires_collision_stopped() {
        let manager = EventManager::default();
        let context = TestContext::default();

        manager
            .intersection_send
            .send(IntersectionEvent::new(
                context.handle1,
                context.handle2,
                false,
            ))
            .unwrap();

        let mut events = Events::<CollisionEvent>::default();
        manager.fire_events(
            &context.narrow_phase,
            &context.bodies,
            &context.colliders,
            &mut events,
        );
        let events: Vec<CollisionEvent> = events.get_reader().iter(&events).cloned().collect();

        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert!(matches!(event, CollisionEvent::Stopped(_, _)));
        assert_eq!(
            event.collision_shape_entities(),
            (context.collider_entity_1, context.collider_entity_2)
        );
    }

    #[test]
    fn contains_rigid_body_entities() {
        let manager = EventManager::default();
        let context = TestContext::default();

        manager
            .contact_send
            .send(ContactEvent::Started(context.handle1, context.handle2))
            .unwrap();

        let mut events = Events::<CollisionEvent>::default();
        manager.fire_events(
            &context.narrow_phase,
            &context.bodies,
            &context.colliders,
            &mut events,
        );
        assert_eq!(
            events
                .get_reader()
                .iter(&events)
                .next()
                .unwrap()
                .rigid_body_entities(),
            (context.rb_entity_1, context.rb_entity_2)
        );
    }

    #[test]
    fn contains_collision_layers() {
        let manager = EventManager::default();
        let context = TestContext::default();

        manager
            .contact_send
            .send(ContactEvent::Started(context.handle1, context.handle2))
            .unwrap();

        let mut events = Events::<CollisionEvent>::default();
        manager.fire_events(
            &context.narrow_phase,
            &context.bodies,
            &context.colliders,
            &mut events,
        );
        assert_eq!(
            events
                .get_reader()
                .iter(&events)
                .next()
                .unwrap()
                .collision_layers(),
            (context.layers_1, context.layers_2)
        );
    }

    /// Marker struct for Ray cast test collider shape
    #[derive(Component)]
    struct RayCastTestCollider;
    fn setup_ray_cast_test_app() -> App {
        fn setup(mut commands: Commands<'_, '_>) {
            // Spawn a block above the world center
            commands.spawn_bundle((
                CollisionShape::Cuboid {
                    half_extends: Vec3::new(10., 10., 10.),
                    border_radius: None,
                },
                RigidBody::Static,
                Transform::from_xyz(0., 100., 0.),
                GlobalTransform::default(),
                RayCastTestCollider,
            ));
        }

        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugin(RapierPlugin)
            .add_startup_system(setup);

        app
    }

    #[test]
    fn ray_cast_hit() {
        /// The system to test ray casting
        fn ray_cast(
            mut runs: Local<'_, i32>,
            physics_world: PhysicsWorld<'_, '_>,
            test_colliders: Query<'_, '_, (), With<RayCastTestCollider>>,
        ) {
            // Skip the first run to give time for the world to setup
            if *runs == 0 {
                *runs += 1;
                return;
            }

            // Cast a ray upword to try and hit the block we spawned with setup_ray_cast_test_app()
            let result = physics_world.ray_cast(Vec3::default(), Vec3::new(0., 200., 0.), true);

            // Verify we hit the block
            if let Some(info) = result {
                // Make sure we hit where we think we should have
                assert!(info.collision_point.distance(Vec3::new(0., 90., 0.)) < 0.1);

                // Make sure we hit the block we think we should have
                assert!(test_colliders.get(info.entity).is_ok());
            } else {
                panic!("Ray cast did not collide when we expected it to");
            }
        }

        // Get the app
        let mut app = setup_ray_cast_test_app();
        // Add our system
        app.add_system(ray_cast);

        // Run the app for a couple of loops to make sure the setup is completed and the ray has been cast
        app.update();
        app.update();
    }

    #[test]
    fn ray_cast_miss() {
        /// The system to test ray casting
        fn ray_cast(mut runs: Local<'_, i32>, physics_world: PhysicsWorld<'_, '_>) {
            // Skip the first run to give time for the world to setup
            if *runs == 0 {
                *runs += 1;
                return;
            }

            // Cast a ray downward to try and miss the block we spawned with setup_ray_cast_test_app()
            let result = physics_world.ray_cast(Vec3::default(), Vec3::new(0., -200., 0.), true);

            // Make sure we don't hit anything
            assert!(result.is_none());
        }

        // Get the app
        let mut app = setup_ray_cast_test_app();
        // Add our system
        app.add_system(ray_cast);

        // Run the app for a couple of loops to make sure the setup is completed and the ray has been cast
        app.update();
        app.update();
    }

    #[test]
    fn shape_cast_hit() {
        /// System to test shape casting
        fn ray_cast(
            mut runs: Local<'_, i32>,
            physics_world: PhysicsWorld<'_, '_>,
            test_colliders: Query<'_, '_, (), With<RayCastTestCollider>>,
        ) {
            // Skip the first run to give time for the world to setup
            if *runs == 0 {
                *runs += 1;
                return;
            }

            // Cast a shape upword to try and hit the block we spawned with setup_ray_cast_test_app()
            let result = physics_world.shape_cast(
                &CollisionShape::Cuboid {
                    half_extends: Vec3::new(10., 10., 10.),
                    border_radius: None,
                },
                Vec3::default(),
                Quat::default(),
                Vec3::new(0., 200., 0.),
            );

            // Verify we hit the block
            if let Some(info) = result {
                // Make sure we hit the block we think we should have
                assert!(test_colliders.get(info.entity).is_ok());

                if let ShapeCastCollisionType::Collided(info) = info.collision_type {
                    // Make sure we hit where we think we should have
                    assert!(info.self_end_position.distance(Vec3::new(0., 80., 0.)) < 0.1);
                } else {
                    panic!("Shape cast did not collide the way we thought it would");
                }
            } else {
                panic!("Shape cast did not collide when we expected it to");
            }
        }

        // Get the app
        let mut app = setup_ray_cast_test_app();
        // Add our system
        app.add_system(ray_cast);

        // Run the app for a couple of loops to make sure the setup is completed and the ray has been cast
        app.update();
        app.update();
    }

    #[test]
    fn shape_cast_miss() {
        /// System to test shape casting
        fn ray_cast(mut runs: Local<'_, i32>, physics_world: PhysicsWorld<'_, '_>) {
            // Skip the first run to give time for the world to setup
            if *runs == 0 {
                *runs += 1;
                return;
            }

            // Cast a shape upword to try and hit the block we spawned with setup_ray_cast_test_app()
            let result = physics_world.shape_cast(
                &CollisionShape::Cuboid {
                    half_extends: Vec3::new(10., 10., 10.),
                    border_radius: None,
                },
                Vec3::default(),
                Quat::default(),
                Vec3::new(0., -200., 0.),
            );

            // Verify we missed the block
            assert!(result.is_none());
        }

        // Get the app
        let mut app = setup_ray_cast_test_app();
        // Add our system
        app.add_system(ray_cast);

        // Run the app for a couple of loops to make sure the setup is completed and the ray has been cast
        app.update();
        app.update();
    }
}
