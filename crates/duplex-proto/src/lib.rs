pub mod crypto;
pub mod hid;
pub mod keymap;
pub mod screen_codec;
pub mod shm;
pub mod state;
pub mod tile_codec;
pub mod video;
pub use screen_codec::{encode_native_bands, DPXC_BAND_MAGIC};
pub use tile_codec::{
    EncodeStats, PixelRect, SliceOutcome, TileDecoder, TileEncoder, TileEncoderConfig, DPXT_MAGIC,
    TILE_SIZE,
};

pub use crypto::{
    ChannelId, CryptoFrameError, CryptoSession, ReplayWindow, WirePayload, VIDEO_CAP_NATIVE_BANDS,
    VIDEO_CAP_TILES,
};
pub use hid::{
    DepressedKeyBitmap, HidEvent, HidMouseButton, HotkeyAction, PanicDetector, PanicTriggerKind,
    ALL_HID_MODIFIER_USAGES, HID_USAGE_BACKSPACE, HID_USAGE_ESCAPE, HID_USAGE_LEFT_ALT,
    HID_USAGE_LEFT_CTRL, HID_USAGE_LEFT_GUI, HID_USAGE_LEFT_SHIFT, HID_USAGE_RIGHT_ALT,
    HID_USAGE_RIGHT_CTRL, HID_USAGE_RIGHT_GUI, HID_USAGE_RIGHT_SHIFT,
};
pub use keymap::{
    hid_to_linux_evdev, hid_to_windows_scancode, linux_evdev_to_hid, windows_scancode_to_hid,
    KEYMAP_TABLE,
};
pub use shm::{
    ShmWatchdogBlock, WatchdogTripReason, LINUX_SHM_NAME, NO_REQUESTED_MODE,
    PEER_HEARTBEAT_INTERVAL_NS, SHM_WATCHDOG_MAGIC, SHM_WATCHDOG_VERSION, WATCHDOG_TIMEOUT_NS,
    WINDOWS_SHM_NAME,
};
pub use state::{
    dscp_to_tos, DisplayDescriptor, ModeStateMachine, NodeRole, OperationalMode, StateMachineError,
    TransitionReason, DEBIAN_LAPTOP_IPV4, DSCP_AF41, DSCP_CS6, DSCP_EF, PORT_CONTROL_TCP,
    PORT_HID_UDP, PORT_VIDEO_STREAM_0_UDP, PORT_VIDEO_STREAM_1_UDP, TOS_CONTROL_CS6, TOS_HID_EF,
    TOS_VIDEO_AF41, WINDOWS_HOST_IPV4,
};
pub use video::{
    decode_band_into_bgrx, emit_video_datagrams, encode_bgrx_frame_to_bands, packetize_video_frame,
    CursorShapeBitmap,
    CursorTelemetry, EncodedVideoFrame, VideoCodec, VideoFragmentHeader, VideoFrameAssembler,
    VideoPacketError, DPXB_BAND_MAGIC, MAX_VIDEO_BITRATE_KBPS, MAX_VIDEO_FRAGMENT_PAYLOAD,
    VIDEO_FLAG_END_OF_FRAME, VIDEO_FLAG_KEYFRAME_IDR, VIDEO_FRAGMENT_HEADER_LEN, VIDEO_FRAME_MAGIC,
};
