use remotevnc_client::video::Viewport;
use remotevnc_protocol::DisplayInfo;
#[test]
fn letterbox_and_retina_use_logical_points_once_and_physical_output_not_decoded_size() {
    let v = Viewport::fit(
        Viewport {
            left: 10.,
            top: 20.,
            width: 1000.,
            height: 1000.,
        },
        1920,
        1080,
    );
    let d = DisplayInfo {
        generation: 1,
        width: 3840,
        height: 2160,
    };
    assert_eq!(v.position(510., 520., d, false), Some((1920, 1080)));
    assert_eq!(v.position(10., 20., d, false), None);
    assert_eq!(v.position(1010., 802., d, true), Some((3839, 2160 - 1)));
    assert_eq!(v.position(f32::NAN, 520., d, true), None);
}
