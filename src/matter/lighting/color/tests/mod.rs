use super::*;

fn close(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
}

#[test]
fn srgb_xy_uses_65536_inverse_gamma_and_deterministic_black() {
    assert_eq!(
        rgb_to_xy(Rgb {
            red: 0,
            green: 255,
            blue: 0
        }),
        Some((19661, 39322))
    );
    assert_eq!(
        rgb_to_xy(Rgb {
            red: 0,
            green: 0,
            blue: 255
        }),
        Some((9830, 3932))
    );
    for y in 1..=6 {
        let rgb = xy_to_rgb(30000, y);
        assert_ne!(
            rgb,
            Rgb {
                red: 0,
                green: 0,
                blue: 0
            }
        );
        assert_eq!(xy_to_rgb(30000, y), rgb);
    }
    assert_eq!(
        rgb_to_xy(Rgb {
            red: 20,
            green: 20,
            blue: 20
        }),
        rgb_to_xy(Rgb {
            red: 255,
            green: 255,
            blue: 255
        })
    );
    assert_eq!(
        rgb_to_xy(Rgb {
            red: 255,
            green: 0,
            blue: 0
        }),
        Some((41943, 21627))
    );
    assert_eq!(
        rgb_to_xy(Rgb {
            red: 255,
            green: 255,
            blue: 255
        }),
        Some((20493, 21561))
    );
    assert_eq!(
        rgb_to_xy(Rgb {
            red: 255,
            green: 128,
            blue: 64
        }),
        Some((33935, 25220))
    );
    assert_eq!(
        rgb_to_xy(Rgb {
            red: 0,
            green: 0,
            blue: 0
        }),
        None
    );
    assert_eq!(
        xy_to_rgb(30000, 0),
        Rgb {
            red: 0,
            green: 0,
            blue: 0
        }
    );
    let color = xy_to_rgb(33935, 25220);
    assert!((color.red as i16 - 255).abs() <= 1);
    assert!((color.green as i16 - 128).abs() <= 1);
    assert!((color.blue as i16 - 64).abs() <= 1);
}

#[test]
fn encoded_srgb_hs_round_trips_color_without_scaling_level() {
    for (packed, hue) in [
        (0xff0000, 0.0),
        (0xffff00, 254.0 / 6.0),
        (0x00ff00, 254.0 / 3.0),
        (0x00ffff, 127.0),
        (0x0000ff, 254.0 * 2.0 / 3.0),
        (0xff00ff, 254.0 * 5.0 / 6.0),
    ] {
        let rgb = Rgb::from_packed(packed).unwrap();
        let hs = rgb_to_hs(rgb);
        close(hs.hue, hue);
        close(hs.saturation, 254.0);
        assert_eq!(hs_to_rgb(hs), rgb);
    }
    for packed in [0xff8040, 0x7fff80, 0x80a0ff, 0xfffe80] {
        let rgb = Rgb::from_packed(packed).unwrap();
        assert_eq!(hs_to_rgb(rgb_to_hs(rgb)), rgb);
    }
    let dimmed = Rgb::from_packed(0x804020).unwrap();
    assert_eq!(
        hs_to_rgb(rgb_to_hs(dimmed)),
        Rgb::from_packed(0xff8040).unwrap()
    );
}

#[test]
fn gray_and_black_are_identifiably_achromatic() {
    for packed in [0, 0x808080, 0xffffff] {
        assert_eq!(
            rgb_to_hs(Rgb::from_packed(packed).unwrap()),
            Hs {
                hue: 0.0,
                saturation: 0.0
            }
        );
    }
    for hue in [0.0, 40.0, 127.0, 254.0] {
        assert_eq!(
            hs_to_rgb(Hs {
                hue,
                saturation: 0.0
            }),
            Rgb::from_packed(0xffffff).unwrap()
        );
    }
    assert_eq!(
        hs_to_rgb(Hs {
            hue: 254.0,
            saturation: 254.0
        }),
        Rgb::from_packed(0xff0000).unwrap()
    );
}
