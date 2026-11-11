use binachicken_client::{
    input::{ClientInput as C, InputMapper},
    video::Viewport,
};
use binachicken_protocol::{DisplayInfo, InputEvent as I, MouseButton};
fn d(generation: u64) -> DisplayInfo {
    DisplayInfo {
        generation,
        width: 200,
        height: 100,
    }
}
fn v() -> Viewport {
    Viewport {
        left: 0.,
        top: 0.,
        width: 100.,
        height: 50.,
    }
}
#[test]
fn composition_is_silent_commit_is_exact_and_intentional_repetition_survives() {
    let mut m = InputMapper::default();
    assert!(
        m.map(C::Composition("Tieengs".into()), v(), d(1))
            .is_empty()
    );
    assert!(
        m.map(
            C::Key {
                keysym: 116,
                pressed: true,
                printable: true,
                shortcut: false
            },
            v(),
            d(1)
        )
        .is_empty()
    );
    for _ in 0..2 {
        let out = m.map(C::Commit("Tiếng Việt".into()), v(), d(1));
        assert!(matches!(&out[..],[I::Text{text}] if text=="Tiếng Việt"));
    }
}
#[test]
fn drag_release_outside_clamps_and_focus_releases_without_new_session() {
    let mut m = InputMapper::default();
    let out = m.map(
        C::Button {
            button: MouseButton::Left,
            pressed: true,
            x: 25.,
            y: 10.,
        },
        v(),
        d(1),
    );
    assert!(matches!(
        &out[..],
        [I::Button {
            pressed: true,
            x: 50,
            y: 20,
            ..
        }]
    ));
    let out = m.map(
        C::Button {
            button: MouseButton::Left,
            pressed: false,
            x: 300.,
            y: 60.,
        },
        v(),
        d(1),
    );
    assert!(matches!(
        &out[..],
        [I::Button {
            pressed: false,
            x: 199,
            y: 99,
            ..
        }]
    ));
    assert!(matches!(
        &m.map(C::FocusLost, v(), d(1))[..],
        [I::ReleaseAll]
    ));
}
#[test]
fn generation_change_releases_and_drops_triggering_input() {
    let mut m = InputMapper::default();
    m.map(
        C::Key {
            keysym: 0xffe3,
            pressed: true,
            printable: false,
            shortcut: true,
        },
        v(),
        d(1),
    );
    assert!(matches!(
        &m.map(C::Pointer { x: 10., y: 10. }, v(), d(2))[..],
        [I::ReleaseAll]
    ));
    assert!(
        m.map(
            C::Key {
                keysym: 0xffe3,
                pressed: false,
                printable: false,
                shortcut: false
            },
            v(),
            d(2)
        )
        .is_empty()
    );
}
#[test]
fn shortcut_keys_release_even_after_modifier_changes() {
    let mut m = InputMapper::default();
    assert!(matches!(
        &m.map(
            C::Key {
                keysym: 99,
                pressed: true,
                printable: true,
                shortcut: true
            },
            v(),
            d(1)
        )[..],
        [I::Key {
            keysym: 99,
            pressed: true
        }]
    ));
    assert!(matches!(
        &m.map(
            C::Key {
                keysym: 99,
                pressed: false,
                printable: true,
                shortcut: false
            },
            v(),
            d(1)
        )[..],
        [I::Key {
            keysym: 99,
            pressed: false
        }]
    ));
}
#[test]
fn native_commits_key_text_pairs_modifiers_and_paste_are_separate() {
    use eframe::egui::{Event as E, ImeEvent, Key, Modifiers};
    let mut m = InputMapper::default();
    let plain = Modifiers::default();
    let events = vec![
        E::Key {
            key: Key::T,
            physical_key: Some(Key::T),
            pressed: true,
            repeat: false,
            modifiers: plain,
        },
        E::Text("t".into()),
        E::Ime(ImeEvent::Preedit {
            text: "Tieengs".into(),
            active_range_chars: None,
        }),
        E::Ime(ImeEvent::Commit("Tiếng Việt".into())),
        E::Text("Tiếng Việt".into()),
        E::Ime(ImeEvent::Commit("Tiếng Việt".into())),
        E::Paste("private clipboard must not travel".into()),
    ];
    let out = m.native_batch(&events, plain, None, v(), d(1));
    let texts: Vec<_> = out
        .iter()
        .filter_map(|e| {
            if let I::Text { text } = e {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(texts, vec!["t", "Tiếng Việt", "Tiếng Việt"]);
    assert!(!out.iter().any(|e| matches!(e, I::Key { keysym: 116, .. })));
    let cmd = Modifiers {
        mac_cmd: true,
        command: true,
        ..plain
    };
    let ctrl = Modifiers {
        ctrl: true,
        command: false,
        ..plain
    };
    let events = vec![
        E::ModifiersChanged(cmd),
        E::Key {
            key: Key::V,
            physical_key: Some(Key::V),
            pressed: true,
            repeat: false,
            modifiers: cmd,
        },
        E::Key {
            key: Key::V,
            physical_key: Some(Key::V),
            pressed: false,
            repeat: false,
            modifiers: cmd,
        },
        E::ModifiersChanged(ctrl),
        E::ModifiersChanged(plain),
    ];
    let out = m.native_batch(&events, plain, None, v(), d(1));
    // Command maps to Control; swapping Cmd for a physical Ctrl never releases it.
    assert!(matches!(
        &out[..],
        [
            I::Key {
                keysym: 0xffe3,
                pressed: true
            },
            I::Key {
                keysym: 118,
                pressed: true
            },
            I::Key {
                keysym: 118,
                pressed: false
            },
            I::Key {
                keysym: 0xffe3,
                pressed: false
            }
        ]
    ));
}

#[test]
fn printable_alt_shortcut_does_not_also_send_framework_text() {
    use eframe::egui::{Event, Key, Modifiers};
    let mut mapper = InputMapper::default();
    let alt = Modifiers {
        alt: true,
        ..Default::default()
    };
    let out = mapper.native_batch(
        &[
            Event::Key {
                key: Key::A,
                physical_key: Some(Key::A),
                pressed: true,
                repeat: false,
                modifiers: alt,
            },
            Event::Text("å".into()),
        ],
        alt,
        None,
        v(),
        d(1),
    );
    assert!(out.iter().any(|event| matches!(
        event,
        I::Key {
            keysym: 97,
            pressed: true
        }
    )));
    assert!(
        !out.iter().any(|event| matches!(event, I::Text { .. })),
        "a physical shortcut cannot also insert native printable text"
    );
}

#[test]
fn trackpad_fractional_scroll_accumulates_and_focus_loss_discards_remainder() {
    use eframe::egui::{Event, Modifiers, MouseWheelUnit, pos2, vec2};
    let mut mapper = InputMapper::default();
    let wheel = Event::MouseWheel {
        unit: MouseWheelUnit::Point,
        delta: vec2(0., 1.),
        phase: eframe::egui::TouchPhase::Move,
        source: eframe::egui::MouseWheelSource::Unknown,
        modifiers: Modifiers::default(),
    };
    let mut steps = 0;
    for _ in 0..40 {
        for event in mapper.native_batch(
            std::slice::from_ref(&wheel),
            Modifiers::default(),
            Some(pos2(10., 10.)),
            v(),
            d(1),
        ) {
            if let I::Scroll { delta_y, .. } = event {
                steps += delta_y;
            }
        }
    }
    assert_eq!(steps, 1);
    for _ in 0..20 {
        mapper.native_batch(
            std::slice::from_ref(&wheel),
            Modifiers::default(),
            Some(pos2(10., 10.)),
            v(),
            d(1),
        );
    }
    mapper.map(C::FocusLost, v(), d(1));
    let mut after = 0;
    for _ in 0..20 {
        for event in mapper.native_batch(
            std::slice::from_ref(&wheel),
            Modifiers::default(),
            Some(pos2(10., 10.)),
            v(),
            d(1),
        ) {
            if let I::Scroll { delta_y, .. } = event {
                after += delta_y;
            }
        }
    }
    assert_eq!(after, 0);
}

#[test]
fn composition_commit_is_not_lost_when_duplicate_text_precedes_commit() {
    use eframe::egui::{Event, ImeEvent, Modifiers};
    let mut mapper = InputMapper::default();
    let events = [
        Event::Ime(ImeEvent::Preedit {
            text: "Tieengs".into(),
            active_range_chars: None,
        }),
        Event::Text("Tiếng Việt".into()),
        Event::Ime(ImeEvent::Commit("Tiếng Việt".into())),
    ];
    let out = mapper.native_batch(&events, Modifiers::default(), None, v(), d(1));
    assert!(matches!(&out[..],[I::Text{text}] if text=="Tiếng Việt"));
}

#[test]
fn egui_content_scroll_sign_is_preserved_on_both_axes() {
    use eframe::egui::{Event, Modifiers, MouseWheelUnit, pos2, vec2};
    for (x, y) in [(1., 0.), (-1., 0.), (0., 1.), (0., -1.)] {
        let mut mapper = InputMapper::default();
        let events = mapper.native_batch(
            &[Event::MouseWheel {
                unit: MouseWheelUnit::Line,
                delta: vec2(x, y),
                phase: eframe::egui::TouchPhase::Move,
                source: eframe::egui::MouseWheelSource::Unknown,
                modifiers: Modifiers::default(),
            }],
            Modifiers::default(),
            Some(pos2(10., 10.)),
            v(),
            d(1),
        );
        assert!(events.iter().any(|event|matches!(event,I::Scroll{delta_x,delta_y,..} if *delta_x==x as i32 && *delta_y==y as i32)));
    }
}
