use pinnacle::{state::WithState, tag::Tag};
use pinnacle_api::{
    layout::{LayoutGenerator as _, generators::MasterStack},
    output::OutputHandle,
};
use smithay::{output::Output, utils::Rectangle};

use crate::common::fixture::Fixture;

fn set_up() -> (Fixture, Output) {
    let mut fixture = Fixture::new();

    let output = fixture.add_output(Rectangle::new((0, 0).into(), (1920, 1080).into()));
    output.with_state_mut(|state| {
        let tag = Tag::new("1".to_string());
        tag.set_active(true);
        state.add_tags([tag]);
    });
    fixture.pinnacle().focus_output(&output);

    fixture
        .runtime_handle()
        .block_on(pinnacle_api::connect())
        .unwrap();

    fixture.spawn_blocking(|| {
        pinnacle_api::layout::manage(|args| pinnacle_api::layout::LayoutResponse {
            root_node: MasterStack::default().layout(args.window_count),
            tree_id: 0,
        });
    });

    (fixture, output)
}

fn set_up_no_layout() -> (Fixture, Output) {
    let mut fixture = Fixture::new();

    let output = fixture.add_output(Rectangle::new((0, 0).into(), (1920, 1080).into()));
    output.with_state_mut(|state| {
        let tag = Tag::new("1".to_string());
        tag.set_active(true);
        state.add_tags([tag]);
    });
    fixture.pinnacle().focus_output(&output);

    fixture
        .runtime_handle()
        .block_on(pinnacle_api::connect())
        .unwrap();

    (fixture, output)
}

#[test_log::test]
fn window_floating_size_heuristic_works() {
    let (mut fixture, _) = set_up();

    let client_id = fixture.add_client();

    fixture.spawn_floating_window_with(client_id, (500, 500), |_| ());

    fixture.spawn_windows(1, client_id).remove(0);

    assert!(fixture.pinnacle().windows[0].with_state(|state| state.layout_mode.is_floating()));

    let size = fixture.pinnacle().windows[0].geometry().size;
    assert_eq!(size, (500, 500).into());
}

#[test_log::test]
fn window_spawned_without_tags_gets_tags_after_add() {
    let (mut fixture, output) = set_up();

    output.with_state_mut(|state| state.tags.clear());

    let id = fixture.add_client();

    // Add a window
    let window = fixture.client(id).create_window();
    window.commit();
    let surface = window.surface();
    fixture.roundtrip(id);

    assert!(
        fixture
            .client(id)
            .window_for_surface(&surface)
            .current_serial()
            .is_none()
    );

    assert!(
        fixture.pinnacle().unmapped_windows[0]
            .window
            .with_state(|state| state.tags.is_empty())
    );

    assert!(fixture.pinnacle().windows.is_empty());
    assert_eq!(fixture.pinnacle().unmapped_windows.len(), 1);

    fixture.spawn_blocking(|| {
        let _ = pinnacle_api::tag::add(
            &pinnacle_api::output::get_focused().unwrap(),
            ["1", "2", "3"],
        );
    });

    assert!(
        fixture
            .client(id)
            .window_for_surface(&surface)
            .current_serial()
            .is_some()
    );

    assert!(
        fixture.pinnacle().unmapped_windows[0]
            .window
            .with_state(|state| !state.tags.is_empty())
    );
}
#[test_log::test]
fn window_tags_update_after_set_geometry() {
    let (mut fixture, output1) = set_up();
    let output2 = fixture.add_output(Rectangle::new((1920, 0).into(), (1920, 1080).into()));
    output2.with_state_mut(|state| {
        let tag = Tag::new("1".to_string());
        tag.set_active(true);
        state.add_tags([tag]);
    });
    fixture.pinnacle().focus_output(&output1);

    let id = fixture.add_client();

    fixture.spawn_floating_window_with(id, (500, 500), |_| ());

    let tags = fixture.pinnacle().windows[0].with_state(|state| state.tags.clone());
    assert_eq!(tags, output1.with_state(|state| state.tags.clone()));

    fixture.spawn_blocking(|| {
        pinnacle_api::window::get_focused()
            .unwrap()
            .set_geometry(2000, None, None, None);
    });

    let tags = fixture.pinnacle().windows[0].with_state(|state| state.tags.clone());
    assert_eq!(tags, output2.with_state(|state| state.tags.clone()));
}

#[test_log::test]
fn windows_spill_correctly() {
    let (mut fixture, _) = set_up_no_layout();

    fixture.spawn_blocking(|| {
        pinnacle_api::layout::manage(|args| pinnacle_api::layout::LayoutResponse {
            root_node: MasterStack::default().layout(args.window_count.min(3)),
            tree_id: 0,
        });
    });

    let id = fixture.add_client();

    fixture.spawn_windows(5, id);

    for (i, win) in fixture.pinnacle().windows.iter().enumerate() {
        match i {
            0..=2 => assert!(win.with_state(|state| state.layout_mode.is_tiled())),
            3..=4 => assert!(win.with_state(|state| state.layout_mode.is_spilled())),
            _ => unreachable!(),
        }
    }
}

#[test_log::test]
fn window_move_to_output() {
    let (mut fixture, output1) = set_up();

    let output_geo = Rectangle::new((1920, 0).into(), (1920, 1080).into());
    let output2 = fixture.add_output(output_geo);
    output2.with_state_mut(|state| {
        let tag = Tag::new("1".to_string());
        tag.set_active(true);
        state.add_tags([tag]);
    });
    fixture.pinnacle().focus_output(&output1);

    let handle = OutputHandle::from_name(output2.name());

    let id = fixture.add_client();
    fixture.spawn_floating_window_with(id, (500, 500), |_| ());

    // sanity checks
    let tags = fixture.pinnacle().windows[0].with_state(|state| state.tags.clone());
    assert_eq!(tags, output1.with_state(|state| state.tags.clone()));
    let win_geo = fixture.pinnacle().windows[0].geometry();
    assert!(!output_geo.overlaps(win_geo));

    fixture.spawn_blocking(move || {
        pinnacle_api::window::get_focused()
            .unwrap()
            .move_to_output(&handle)
    });

    // check tags were updated
    let tags = fixture.pinnacle().windows[0].with_state(|state| state.tags.clone());
    assert_eq!(tags, output2.with_state(|state| state.tags.clone()));
}

#[test_log::test]
fn window_floating_pick_size() {
    let (mut fixture, ..) = set_up();

    let client_id = fixture.add_client();

    fixture.spawn_blocking(move || {
        pinnacle_api::window::add_window_rule(|win| {
            win.set_floating(true);
        });
    });

    fixture.spawn_floating_window_with(client_id, (500, 500), |win| {
        win.size_configure_hook(|size| {
            assert_eq!(size, (0, 0));
            false // we only care about initial configure. Unhook ourselves.
        });
    });

    let size = fixture.pinnacle().windows[0].geometry().size;
    assert_eq!(size, (500, 500).into());
}

/// A window that maps while its tag is hidden keeps the size its client chose, because the
/// compositor only lays out the tags an output is showing. The first visit to that tag is the
/// first time the window is configured to a tile size, and a real client acknowledges that
/// configure before it repaints. The compositor must not show the window in between: doing so
/// flashes the old buffer at the new place for a frame or two.
#[test_log::test]
fn first_visit_to_a_tag_does_not_show_the_window_at_its_old_size() {
    let (mut fixture, output) = set_up();

    let tag2 = Tag::new("2".to_string());
    output.with_state_mut(|state| state.add_tags([tag2.clone()]));

    let id = fixture.add_client();

    // Map a tiled window while its tag is hidden, and have the client acknowledge configures
    // without resizing, the way a real toolkit does before it repaints.
    let window = fixture.client(id).create_window();
    window.apply_configure_size = false;
    window.set_app_id("late-paint");
    window.commit();
    let surface = window.surface();
    fixture.roundtrip(id);

    fixture.pinnacle().unmapped_windows[0]
        .window
        .with_state_mut(|state| state.tags = [tag2.clone()].into_iter().collect());

    let window = fixture.client(id).window_for_surface(&surface);
    window.attach_buffer();
    window.ack_and_commit();
    fixture.roundtrip(id);

    let win = fixture.pinnacle().windows[0].clone();
    let old_size = win.geometry().size;
    assert!(fixture.pinnacle().space.element_location(&win).is_none());

    // Drain the configure the map itself produced.
    fixture.client(id).ack_all_window();
    fixture.roundtrip(id);
    fixture.dispatch();

    // First visit to the tag: the layout assigns the window a tile.
    pinnacle::api::tag::switch_to(fixture.state(), &tag2);
    fixture.dispatch_until(|fixture| !fixture.pinnacle().layout_state.layout_trees.is_empty());
    fixture.wait_client_configure(id);

    let tile_size = fixture
        .client(id)
        .window_for_surface(&surface)
        .last_configure_size()
        .expect("window was configured to a tile size");
    assert_ne!(tile_size, (old_size.w, old_size.h));

    // The client acknowledges the configure but has not painted the new size yet.
    fixture.client(id).ack_all_window();
    fixture.roundtrip(id);
    fixture.dispatch();

    assert!(
        fixture.pinnacle().space.element_location(&win).is_none(),
        "window was mapped at its old size {old_size:?} before painting the configured {tile_size:?}"
    );

    // It paints at the configured size.
    let window = fixture.client(id).window_for_surface(&surface);
    window.set_size(tile_size.0, tile_size.1);
    window.commit();
    fixture.roundtrip(id);
    fixture.dispatch();

    assert!(
        fixture.pinnacle().space.element_location(&win).is_some(),
        "window stayed unmapped after painting its configured size"
    );
    assert_eq!(win.geometry().size, tile_size.into());

    // Coming back needs no configure: the window is already at its tile size, so the switch must
    // not wait for anything.
    let tag1 = output
        .with_state(|state| state.tags.first().cloned())
        .expect("set_up leaves one tag on the output");

    pinnacle::api::tag::switch_to(fixture.state(), &tag1);
    fixture.dispatch_until(|fixture| fixture.pinnacle().space.element_location(&win).is_none());

    pinnacle::api::tag::switch_to(fixture.state(), &tag2);
    fixture.dispatch_until(|fixture| fixture.pinnacle().space.element_location(&win).is_some());
}
