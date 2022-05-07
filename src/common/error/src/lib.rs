pub mod ext;
pub mod format;
pub mod status_code;

pub mod prelude {
    pub use snafu::{prelude::*, Backtrace, ErrorCompat};

    pub use crate::ext::ErrorExt;
    pub use crate::format::DebugFormat;
    pub use crate::status_code::StatusCode;
}

pub use snafu;
