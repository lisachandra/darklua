mod module_definitions;

use module_definitions::BuildModuleDefinitions;

use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::{iter, mem};

use crate::frontend::{ContentType, DarkluaResult};
use crate::nodes::{
    Block, DoStatement, Expression, FunctionCall, Prefix, Statement, VariableAssignment,
};
use crate::process::{DefaultVisitor, IdentifierTracker, NodeProcessor, NodeVisitor, ScopeVisitor};
use crate::rules::require::{is_require_call, match_path_require_call, PathLocator};
use crate::rules::{
    Context, ContextBuilder, FlawlessRule, ReplaceReferencedTokens, RuleProcessResult,
};
use crate::utils::Timer;
use crate::{DarkluaError, Resources};

use super::BundleOptions;

pub(crate) enum RequiredResource {
    Block(Block),
    Expression(Expression),
}

#[derive(Debug)]
struct RequirePathProcessor<'a, 'resources, PathLocatorImpl> {
    options: &'a BundleOptions,
    identifier_tracker: IdentifierTracker,
    path_locator: PathLocatorImpl,
    module_definitions: BuildModuleDefinitions,
    source: PathBuf,
    module_cache: HashMap<PathBuf, Expression>,
    require_stack: Vec<PathBuf>,
    skip_module_paths: HashSet<PathBuf>,
    resources: &'resources Resources,
    errors: Vec<String>,
}

impl<'a, 'b, 'resources, PathLocatorImpl: PathLocator + Clone>
    RequirePathProcessor<'a, 'resources, PathLocatorImpl>
{
    fn new<'context, 'code>(
        context: &'context Context<'b, 'resources, 'code>,
        options: &'a BundleOptions,
        path_locator: PathLocatorImpl,
    ) -> Self
    where
        'context: 'b,
        'context: 'resources,
        'context: 'code,
    {
        Self {
            options,
            identifier_tracker: IdentifierTracker::new(),
            path_locator,
            module_definitions: BuildModuleDefinitions::new(options.modules_identifier()),
            source: context.current_path().to_path_buf(),
            module_cache: Default::default(),
            require_stack: Default::default(),
            skip_module_paths: Default::default(),
            resources: context.resources(),
            errors: Vec::new(),
        }
    }

    fn apply(self, block: &mut Block, context: &Context) -> RuleProcessResult {
        self.module_definitions.apply(block, context);
        match self.errors.len() {
            0 => Ok(()),
            1 => Err(self.errors.first().unwrap().to_string()),
            _ => Err(format!("- {}", self.errors.join("\n- "))),
        }
    }

    fn require_call(&self, call: &FunctionCall) -> Option<PathBuf> {
        // First check for custom require calls (e.g., TS.import, Roblox requires)
        if let Some(path) = self.path_locator.resolve_require_call(call, &self.source) {
            return Some(path);
        }

        if is_require_call(call, self) {
            match_path_require_call(call)
                .or_else(|| self.path_locator.resolve_require_path(call, &self.source))
        } else {
            None
        }
    }

    /// Decides whether a resolved path stays a runtime require instead of being inlined.
    ///
    /// Two independent signals are checked.
    ///
    /// The configured exclude globs are matched first, against the resolved path and
    /// against a project-relative spelling of it: a resolved path may be rebased to the
    /// requiring file (`./../..`), which no project-relative glob can match.
    ///
    /// Then the locator is asked whether the file is a dependency. This is the reliable
    /// signal for vendored and workspace packages, because a path glob cannot identify
    /// them: workspace links are junctions on Windows and symlinks elsewhere, so
    /// `packages/@lisachandra/core/out/logger.luau` and
    /// `../../Packages/rbxts/packages/core/out/logger.luau` are the same file under two
    /// names. Whichever spelling the resolver produced, the instance tree still mounts
    /// the module under a `node_modules` folder, and project code never is.
    fn is_excluded(&self, require_path: &Path) -> bool {
        if self.options.is_excluded(require_path) {
            return true;
        }

        if self.path_locator.is_dependency(require_path) {
            return true;
        }

        match project_relative_form(require_path, &self.source) {
            Some(project_relative) => self.options.is_excluded(&project_relative),
            None => false,
        }
    }

    fn try_inline_call(&mut self, call: &FunctionCall) -> Option<Expression> {
        let literal_require_path = self.require_call(call)?;

        /*
         * Exclude globs are written project-relative, but the literal path is whatever
         * the resolver produced: a `.`-prefixed, `..`-walking path rebased onto the
         * requiring file. Testing only that form is why no exclude pattern could stop
         * the workspace packages from being inlined (#851). The resolved path is tested
         * again below, where it is normalized and can be matched against the sourcemap.
         */
        if self.options.is_excluded(&literal_require_path) {
            log::info!(
                "exclude `{}` from bundle [from `{}`]",
                literal_require_path.display(),
                self.source.display()
            );
            return None;
        }

        let require_path = match self
            .path_locator
            .find_require_path(&literal_require_path, &self.source)
        {
            Ok(path) => path,
            Err(err) => {
                self.errors.push(err.to_string());
                return None;
            }
        };

        log::debug!(
            "found require call to path `{}` (normalized `{}`)",
            literal_require_path.display(),
            require_path.display()
        );

        if self.is_excluded(&require_path) {
            log::info!(
                "exclude `{}` from bundle [from `{}`]",
                require_path.display(),
                self.source.display()
            );
            return None;
        }

        if self.skip_module_paths.contains(&require_path) {
            log::trace!(
                "skip `{}` because it previously errored",
                require_path.display()
            );
            return None;
        }

        match self.inline_require(&require_path, call) {
            Ok(expression) => Some(expression),
            Err(error) => {
                self.errors.push(error.to_string());
                self.skip_module_paths.insert(require_path);
                None
            }
        }
    }

    fn inline_require(
        &mut self,
        require_path: &Path,
        call: &FunctionCall,
    ) -> DarkluaResult<Expression> {
        if let Some(expression) = self.module_cache.get(require_path) {
            Ok(expression.clone())
        } else {
            if let Some(i) = self
                .require_stack
                .iter()
                .enumerate()
                .find(|(_, path)| **path == require_path)
                .map(|(i, _)| i)
            {
                let require_stack_paths: Vec<_> = self
                    .require_stack
                    .iter()
                    .skip(i)
                    .map(|path| path.display().to_string())
                    .chain(iter::once(require_path.display().to_string()))
                    .collect();

                return Err(DarkluaError::custom(format!(
                    "cyclic require detected with `{}`",
                    require_stack_paths.join("` > `")
                )));
            }

            self.require_stack.push(require_path.to_path_buf());
            let required_resource = self.require_resource(require_path);
            self.require_stack.pop();

            let module_value = self.module_definitions.build_module_from_resource(
                required_resource?,
                require_path,
                call,
            )?;

            self.module_cache
                .insert(require_path.to_path_buf(), module_value.clone());

            Ok(module_value)
        }
    }

    fn require_resource(&mut self, path: impl AsRef<Path>) -> DarkluaResult<RequiredResource> {
        let path = path.as_ref();
        log::trace!("look for resource `{}`", path.display());
        if !self.resources.exists(path).map_err(DarkluaError::from)? {
            return Err(DarkluaError::resource_not_found(path));
        }

        let loader = self.options.loaders().get_loader(path).to_internal_loader();

        match loader.load(path, self.resources, self.options.parser())? {
            ContentType::None => Err(DarkluaError::require_unknown_resource(path)),
            ContentType::Copied(_data) => Err(DarkluaError::require_copied_resource(path)),
            ContentType::Block(block) => Ok(RequiredResource::Block(block)),
            ContentType::Expression(expression) => Ok(RequiredResource::Expression(expression)),
            ContentType::Parsed { mut block, source } => {
                if self.options.parser().is_preserving_tokens() {
                    log::trace!("replacing token references of {}", path.display());
                    let context = ContextBuilder::new(path, self.resources, &source).build();
                    // run `replace_referenced_tokens` rule to avoid generating invalid code
                    // when using the token-based generator
                    let replace_tokens = ReplaceReferencedTokens::default();

                    let apply_replace_tokens_timer = Timer::now();

                    replace_tokens.flawless_process(&mut block, &context);

                    log::trace!(
                        "replaced token references for `{}` in {}",
                        path.display(),
                        apply_replace_tokens_timer.duration_label()
                    );
                }

                let current_source = mem::replace(&mut self.source, path.to_path_buf());

                let apply_processor_timer = Timer::now();
                DefaultVisitor::visit_block(&mut block, self);

                log::debug!(
                    "processed `{}` into bundle in {}",
                    path.display(),
                    apply_processor_timer.duration_label()
                );

                self.source = current_source;

                Ok(RequiredResource::Block(block))
            }
        }
    }
}

impl<'a, 'resources, PathLocatorImpl: PathLocator> Deref
    for RequirePathProcessor<'a, 'resources, PathLocatorImpl>
{
    type Target = IdentifierTracker;

    fn deref(&self) -> &Self::Target {
        &self.identifier_tracker
    }
}

impl<'a, 'resources, PathLocatorImpl: PathLocator> DerefMut
    for RequirePathProcessor<'a, 'resources, PathLocatorImpl>
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.identifier_tracker
    }
}

impl<'a, 'resources, PathLocatorImpl: PathLocator + Clone> NodeProcessor
    for RequirePathProcessor<'a, 'resources, PathLocatorImpl>
{
    fn process_expression(&mut self, expression: &mut Expression) {
        if let Expression::Call(call) = expression {
            if let Some(replace_with) = self.try_inline_call(call) {
                *expression = replace_with;
            }
        }
    }

    fn process_prefix_expression(&mut self, prefix: &mut Prefix) {
        if let Prefix::Call(call) = prefix {
            if let Some(replace_with) = self.try_inline_call(call) {
                *prefix = replace_with.into();
            }
        }
    }

    fn process_statement(&mut self, statement: &mut Statement) {
        if let Statement::Call(call) = statement {
            if let Some(replace_with) = self.try_inline_call(call) {
                if let Expression::Call(replace_with) = replace_with {
                    *call = *replace_with;
                } else {
                    *statement = convert_expression_to_statement(replace_with);
                }
            }
        }
    }
}

fn convert_expression_to_statement(expression: Expression) -> Statement {
    DoStatement::new(
        Block::default()
            .with_statement(VariableAssignment::from_variable("_").with_value(expression)),
    )
    .into()
}

/// Re-express a source-relative path as project-relative.
///
/// The resolved path handed to `is_excluded` is relative to the *requiring file*
/// (`HybridRequireMode::to_require_path` rebases it that way so `find_require_path`
/// can resolve it). Undoing that means joining it back onto the requiring file's
/// directory and normalizing the `..` runs away.
///
/// Returns `None` when the path is not dot-relative: an absolute path, or one already
/// anchored at a configured source alias (`node_modules/...`, `out/...`), is already
/// in the form the exclude globs are written against.
fn project_relative_form(path: &Path, source: &Path) -> Option<PathBuf> {
    if !matches!(
        path.components().next(),
        Some(std::path::Component::CurDir | std::path::Component::ParentDir)
    ) {
        return None;
    }

    /*
     * Both sides are normalized without keeping `.` components, mirroring
     * `rebase_to_source`. A source like `.\out\shared\network\match.luau` carries a
     * leading `.` that would otherwise survive the join: the rebase counted four
     * parent hops, but the un-normalized base has five components, so the result was
     * a `.\`-prefixed path that no project-relative glob can match.
     */
    let mut base = crate::utils::normalize_path(source);
    base.pop();

    Some(crate::utils::normalize_path(base.join(path)))
}

pub(crate) fn process_block(
    block: &mut Block,
    context: &Context,
    options: &BundleOptions,
    locator: impl PathLocator + Clone,
) -> Result<(), String> {
    if options.parser().is_preserving_tokens() {
        log::trace!(
            "replacing token references of {}",
            context.current_path().display()
        );
        let replace_tokens = ReplaceReferencedTokens::default();

        let apply_replace_tokens_timer = Timer::now();

        replace_tokens.flawless_process(block, context);

        log::trace!(
            "replaced token references for `{}` in {}",
            context.current_path().display(),
            apply_replace_tokens_timer.duration_label()
        );
    }

    let mut processor = RequirePathProcessor::new(context, options, locator);
    ScopeVisitor::visit_block(block, &mut processor);
    processor.apply(block, context)
}
