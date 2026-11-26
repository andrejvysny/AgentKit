//! Guest and launcher wire protocol of the Track B sandbox backend
//! (ADR 0039, layer 1): the frame codec and the message types. No async
//! runtime and no I/O; callers read and write frames over their own streams.
//!
//! - [`frame`]: the length-prefixed frame codec, incremental and blocking.
//! - [`stream`]: byte streams split over data frames.
//! - [`guest`]: host and supervisor messages.
//! - [`launcher`]: adapter and launcher messages.
//! - [`validate`]: id and digest checks and the documented bounds.

mod envelope;
pub mod frame;
pub mod guest;
pub mod launcher;
pub mod stream;
pub mod validate;

pub use envelope::Envelope;
pub use frame::{
    CONTROL_MAX_BYTES, DATA_MAX_BYTES, DataFrame, Frame, FrameDecoder, FrameError,
    PROTOCOL_VERSION, encode_control, encode_data, parse_control, read_frame, write_frame,
};
pub use stream::{DataReceiver, StreamError, data_frames};
pub use validate::{
    MACHINE_DIR_NAME_BYTES, machine_dir_name, valid_operation_id, valid_sha256, valid_workspace_id,
};
