use remotevnc_protocol::*;
use std::io::Cursor;

#[test]
fn binary_video_preserves_access_unit_metadata_and_bounds() {
    let frame = EncodedFrame {
        generation: 9,
        pts_ns: 123456,
        keyframe: true,
        data: vec![0, 0, 0, 1, 0x65, 17],
    };
    let mut bytes = Vec::new();
    write_video_frame(&mut bytes, &frame).unwrap();
    assert_eq!(read_video_frame(&mut Cursor::new(bytes)).unwrap(), frame);
    let mut malicious = Vec::from(*b"RVF1");
    malicious.extend_from_slice(&((ENCODED_FRAME_LIMIT + 1) as u32).to_be_bytes());
    assert!(read_video_frame(&mut Cursor::new(malicious)).is_err());
}
#[test]
fn profile_bounds_and_pressure_downshift() {
    let p = VideoProfile::default();
    assert_eq!((p.width, p.height, p.fps), (1920, 1080, 60));
    p.validate().unwrap();
    let lower = p.downshift();
    assert!(lower.bitrate_kbps < p.bitrate_kbps);
    assert!(lower.width < p.width && lower.height < p.height && lower.fps < p.fps);
    let mut minimum = p;
    for _ in 0..100 {
        minimum = minimum.downshift();
        minimum.validate().unwrap();
    }
    assert_eq!(minimum, minimum.downshift());
    assert!(
        VideoProfile {
            width: u32::MAX,
            ..p
        }
        .validate()
        .is_err()
    );
}

#[test]
fn tagged_helper_output_keeps_json_and_binary_separate() {
    let mut wire = Vec::new();
    write_helper_output(&mut wire, &HelperOutput::Event(HelperEvent::Ready)).unwrap();
    let frame = EncodedFrame {
        generation: 3,
        pts_ns: 5,
        keyframe: true,
        data: vec![1, 2, 3],
    };
    write_helper_output(&mut wire, &HelperOutput::Video(frame.clone())).unwrap();
    let mut reader = Cursor::new(wire);
    assert!(matches!(
        read_helper_output(&mut reader).unwrap(),
        HelperOutput::Event(HelperEvent::Ready)
    ));
    assert!(matches!(read_helper_output(&mut reader).unwrap(),HelperOutput::Video(f) if f==frame));
    for word in [0u32, 0x80000000, 0xffffffff, 65537] {
        assert!(read_helper_output(&mut Cursor::new(word.to_be_bytes())).is_err());
    }
}

#[test]
fn binary_video_rejects_unknown_magic_flags_and_length_mismatch() {
    let frame = EncodedFrame {
        generation: 1,
        pts_ns: 2,
        keyframe: true,
        data: vec![0, 0, 1, 5],
    };
    let mut bytes = Vec::new();
    write_video_frame(&mut bytes, &frame).unwrap();
    let mut bad = bytes.clone();
    bad[3] = b'2';
    assert!(read_video_frame(&mut Cursor::new(bad)).is_err());
    let mut bad = bytes.clone();
    bad[24] = 2;
    assert!(read_video_frame(&mut Cursor::new(bad)).is_err());
    let mut envelope = (0x80000000u32 | (bytes.len() + 1) as u32)
        .to_be_bytes()
        .to_vec();
    envelope.extend(bytes);
    assert!(read_helper_output(&mut Cursor::new(envelope)).is_err());
}
