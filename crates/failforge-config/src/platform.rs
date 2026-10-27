//! Native executable names shared by configuration and its consumers.
use std::borrow::Cow;

pub const DART_BIN: &str = if cfg!(windows) {
    "bin/dart.exe"
} else {
    "bin/dart"
};
pub const FLUTTER_BIN: &str = if cfg!(windows) {
    "bin/flutter.bat"
} else {
    "bin/flutter"
};
pub const FLUTTER_TESTER: &str = if cfg!(windows) {
    "flutter_tester.exe"
} else {
    "flutter_tester"
};

pub fn binary_name(name: &str) -> Cow<'_, str> {
    if cfg!(windows) {
        Cow::Owned(format!("{name}.exe"))
    } else {
        Cow::Borrowed(name)
    }
}
