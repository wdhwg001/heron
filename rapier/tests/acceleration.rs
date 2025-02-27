#![cfg(any(dim2, dim3))]

use std::time::Duration;

use bevy::core::CorePlugin;
use bevy::prelude::*;
use bevy::prelude::{GlobalTransform, Transform};
use bevy::reflect::TypeRegistryArc;

use heron_core::{Acceleration, AxisAngle, CollisionShape, PhysicsSteps, RigidBody};
use heron_rapier::convert::{IntoBevy, IntoRapier};
use heron_rapier::RapierPlugin;
use utils::*;

mod utils;

fn test_app() -> App {
    let mut builder = App::new();
    builder
        .init_resource::<TypeRegistryArc>()
        .insert_resource(PhysicsSteps::every_frame(Duration::from_secs(1)))
        .add_plugin(CorePlugin)
        .add_plugin(RapierPlugin);
    builder
}

#[test]
fn body_is_created_with_acceleration() {
    let mut app = test_app();

    #[cfg(dim3)]
    let linear = Vec3::new(1.0, 2.0, 3.0);
    #[cfg(dim2)]
    let linear = Vec3::new(1.0, 2.0, 0.0);

    let angular = AxisAngle::new(Vec3::Z, 1.0);

    let entity = app
        .world
        .spawn()
        .insert_bundle((
            Transform::default(),
            GlobalTransform::default(),
            RigidBody::Dynamic,
            CollisionShape::Sphere { radius: 1.0 },
            Acceleration { linear, angular },
        ))
        .id();

    app.update();

    {
        let bodies = app.world.resource::<RigidBodySet>();

        let body = bodies
            .get(
                app.world
                    .get::<heron_rapier::RigidBodyHandle>(entity)
                    .unwrap()
                    .into_rapier(),
            )
            .unwrap();

        println!("{:?}", body);
        assert_eq!(body.linvel().into_bevy(), Vec3::ZERO);
        assert_eq_angular(body.angvel(), AxisAngle::from(Vec3::ZERO));
    }

    app.update();

    let bodies = app.world.resource::<RigidBodySet>();

    let body = bodies
        .get(
            app.world
                .get::<heron_rapier::RigidBodyHandle>(entity)
                .unwrap()
                .into_rapier(),
        )
        .unwrap();

    println!("{:?}", body);
    assert_eq!(body.linvel().into_bevy(), linear);
    assert_eq_angular(body.angvel(), angular);
}

#[test]
fn acceleration_may_be_added_after_creating_the_body() {
    let mut app = test_app();

    let entity = app
        .world
        .spawn()
        .insert_bundle((
            Transform::default(),
            GlobalTransform::default(),
            RigidBody::Dynamic,
            CollisionShape::Sphere { radius: 1.0 },
        ))
        .id();

    app.update();

    #[cfg(dim3)]
    let linear = Vec3::new(1.0, 2.0, 3.0);
    #[cfg(dim2)]
    let linear = Vec3::new(1.0, 2.0, 0.0);

    let angular = AxisAngle::new(Vec3::Z, 2.0);

    app.world
        .entity_mut(entity)
        .insert(Acceleration { linear, angular });

    app.update();

    let bodies = app.world.resource::<RigidBodySet>();

    let body = bodies
        .get(
            app.world
                .get::<heron_rapier::RigidBodyHandle>(entity)
                .unwrap()
                .into_rapier(),
        )
        .unwrap();

    assert_eq!(body.linvel().into_bevy(), linear);
    assert_eq_angular(body.angvel(), angular);
}

#[cfg(dim3)]
fn assert_eq_angular(actual: &Vector<f32>, expected: AxisAngle) {
    assert_eq!(actual.into_bevy(), expected.into());
}

#[cfg(dim2)]
fn assert_eq_angular(expected: f32, actual: AxisAngle) {
    assert!(
        (expected - actual.angle()).abs() < 0.00001,
        "actual rapier angle ({}) doesn't match expected axis-angle: {:?}",
        expected,
        actual
    );
}
