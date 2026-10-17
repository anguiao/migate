use super::{
    repair::{repair_ready, repair_rig},
    *,
};
use crate::device::{LightMode, Rgb};

#[test]
fn representative_cannot_mix_a_new_source_mode_with_an_old_rgb_cache() {
    let rig = repair_rig(1, false);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        until(|| {
            ["member-a", "member-b"].iter().all(|did| {
                function(&c.context.registry, did, "main")
                    .snapshot()
                    .lighting
                    .is_some_and(|state| state.mode.is_some() && state.color.is_some())
            })
        })
        .await;
        let light = function(&c.context.registry, "group-1", "main");
        let work = c.context.live.borrow().devices["member-a"]
            .work
            .clone()
            .unwrap();
        let color = crate::xiaomi::capability::PropertyBinding { siid: 4, piid: 1 };
        let mode = crate::xiaomi::capability::PropertyBinding { siid: 4, piid: 2 };
        work.notify_value(color, &json!(0x00ff00), false, false);
        c.context.refresh_repair_groups("member-a");
        assert_eq!(
            light.snapshot().lighting.unwrap().color,
            Rgb::from_packed(0x00ff00)
        );

        // Only a mode has arrived from the new source authority. Its missing
        // RGB cannot be filled with the previous authority's cached value.
        work.source_changed();
        work.notify_value(mode, &json!(0), false, false);
        c.context.refresh_repair_groups("member-a");
        let state = light.snapshot().lighting.unwrap();
        assert_eq!(state.mode, Some(LightMode::Temperature));
        assert_eq!(state.color, None);

        // A readable unknown-mode A and a valid but OFF B are in the same
        // fallback tier, so stable member order selects A and keeps it unknown.
        work.source_changed();
        work.notify_value(
            crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
            &json!(true),
            false,
            false,
        );
        c.context.refresh_repair_groups("member-a");
        let state = light.snapshot().lighting.unwrap();
        assert_eq!(state.mode, None);
        assert_eq!(state.color, None);
    }));
}
