mod luau_path_locator;
mod hybrid_require_mode;

mod luau_require_mode;
mod match_require;
mod path_iterator;
mod path_locator;
mod path_require_mode;
pub(crate) mod path_utils;

use std::path::{Path, PathBuf};

pub(crate) use luau_path_locator::LuauPathLocator;
pub use luau_require_mode::LuauRequireMode;
pub(crate) use match_require::{is_require_call, match_path_require_call};
pub(crate) use path_locator::RequirePathLocator;
pub use path_require_mode::PathRequireMode;
pub(crate) use hybrid_require_mode::HybridRequireMode;


use crate::nodes::FunctionCall;

use crate::DarkluaError;

pub(crate) trait PathLocator {
    /// Try to resolve a custom require call (e.g., TS.import, Roblox-style requires).
    /// Returns the resolved path if recognized, None otherwise.
    fn resolve_require_call(&self, _call: &FunctionCall, _source: &Path) -> Option<PathBuf> {
        None
    }


    /// Try to resolve a Roblox-style require path (e.g., require(script.Parent.X)).
    /// Called only when `is_require_call` returns true but standard path matching fails.
    fn resolve_require_path(&self, _call: &FunctionCall, _source: &Path) -> Option<PathBuf> {
        None
    }


    fn find_require_path(
        &self,
        path: impl Into<PathBuf>,
        source: &Path,
    ) -> Result<PathBuf, DarkluaError>;


}
