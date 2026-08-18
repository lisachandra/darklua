use std::path::{Path, PathBuf};

use crate::{
    nodes::{Block, FunctionCall},
    rules::{
        bundle::{path_require_mode, BundleOptions},
        require::{HybridRequireMode, PathLocator, RequirePathLocator},
        Context,
    },
    DarkluaError, Resources,
};

#[derive(Clone, Debug)]
pub(crate) struct HybridPathLocator<'a, 'resources> {
    hybrid_require_mode: HybridRequireMode,
    extra_module_relative_location: &'a Path,
    resources: &'resources Resources,
}

impl<'a, 'resources> HybridPathLocator<'a, 'resources> {
    pub(crate) fn new(
        hybrid_require_mode: HybridRequireMode,
        extra_module_relative_location: &'a Path,
        resources: &'resources Resources,
    ) -> Self {
        Self {
            hybrid_require_mode,
            extra_module_relative_location,
            resources,
        }
    }
}

impl PathLocator for HybridPathLocator<'_, '_> {
    fn find_require_path(
        &self,
        path: impl Into<PathBuf>,
        source: &Path,
    ) -> Result<PathBuf, DarkluaError> {
        let locator = RequirePathLocator::new(
            self.hybrid_require_mode.path_require_mode().clone(),
            self.extra_module_relative_location,
            self.resources,
        );
        locator.find_require_path(path, source)
    }

    fn resolve_require_call(&self, call: &FunctionCall, source: &Path) -> Option<PathBuf> {
        self.hybrid_require_mode.resolve_require_call(call, source)
    }

    fn resolve_require_path(&self, call: &FunctionCall, source: &Path) -> Option<PathBuf> {
        self.hybrid_require_mode.resolve_require_path(call, source)
    }
}

pub(crate) fn process_block(
    block: &mut Block,
    context: &Context,
    options: &BundleOptions,
    hybrid_require_mode: &HybridRequireMode,
) -> Result<(), String> {
    let locator = HybridPathLocator::new(
        hybrid_require_mode.clone(),
        context.project_location(),
        context.resources(),
    );
    path_require_mode::process_block(block, context, options, locator)
}
