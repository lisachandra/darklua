use core::option::Option::None;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    frontend::DarkluaResult,
    nodes::{Arguments, Expression, FunctionCall, Prefix},
    rules::{
        convert_require::{instance_path::InstancePath, rojo_sourcemap::RojoSourcemap},
        parse_roblox, Context,
    },
    utils, DarkluaError,
};

use super::PathRequireMode;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct HybridRequireMode {
    #[serde(flatten)]
    path_require_mode: PathRequireMode,

    #[serde(default)]
    convert_ts_imports: bool,

    #[serde(skip)]
    cached_sourcemap: Option<RojoSourcemap>,

    rojo_sourcemap: Option<PathBuf>,
}

impl HybridRequireMode {
    pub(crate) fn initialize(&mut self, context: &Context) -> DarkluaResult<()> {
        if let Some(ref rojo_sourcemap_path) = self
            .rojo_sourcemap
            .as_ref()
            .map(|rojo_sourcemap_path| context.project_location().join(rojo_sourcemap_path))
        {
            context.add_file_dependency(rojo_sourcemap_path.clone());

            let sourcemap_parent_location = get_relative_parent_path(rojo_sourcemap_path);
            let sourcemap = RojoSourcemap::parse(
                &context
                    .resources()
                    .get(rojo_sourcemap_path)
                    .map_err(|err| {
                        DarkluaError::from(err).context("while initializing Roblox require mode")
                    })?,
                sourcemap_parent_location,
            )
            .map_err(|err| {
                err.context(format!(
                    "unable to parse Rojo sourcemap at `{}`",
                    rojo_sourcemap_path.display()
                ))
            })?;
            self.cached_sourcemap = Some(sourcemap);
        }
        Ok(())
    }
}

impl HybridRequireMode {
    pub(crate) fn path_require_mode(&self) -> &PathRequireMode {
        &self.path_require_mode
    }

    pub(crate) fn resolve_require_call(
        &self,
        call: &FunctionCall,
        source: &Path,
    ) -> Option<PathBuf> {
        self.resolve_ts_import(call, source)
            .or_else(|| self.resolve_roblox_require(call, source))
    }

    pub(crate) fn resolve_require_path(
        &self,
        call: &FunctionCall,
        source: &Path,
    ) -> Option<PathBuf> {
        self.resolve_roblox_require(call, source)
    }

    /// Reports whether the given file is mounted under a `node_modules` node.
    ///
    /// The bundler asks this before inlining. A path glob cannot answer it because
    /// workspace links (junctions on Windows, symlinks elsewhere) make one file
    /// reachable under several spellings, so the instance tree is the authority.
    pub(crate) fn is_dependency(&self, path: &Path) -> bool {
        self.cached_sourcemap
            .as_ref()
            .is_some_and(|sourcemap| sourcemap.is_under_node_modules(path))
    }

    fn resolve_ts_import(&self, call: &FunctionCall, source: &Path) -> Option<PathBuf> {
        if !self.convert_ts_imports {
            return None;
        }

        let Prefix::Field(field) = call.get_prefix() else {
            return None;
        };
        match field.get_prefix() {
            Prefix::Identifier(x) if x.get_name() == "TS" && x.get_token().is_none() => Some(()),
            _ => None,
        }?;
        if !(field.get_field().get_name() == "import" && field.get_field().get_token().is_none()) {
            return None;
        }

        let Arguments::Tuple(values) = call.get_arguments() else {
            return None;
        };
        if values.iter_values().count() == 0 {
            return None;
        };

        let mut values_iter = values.iter_values();
        values_iter.next();

        // The second argument locates the module: a service (`game:GetService("X")`),
        // a `script`-relative expression such as `script.Parent`, or nothing (the
        // DataModel root). A `TS.getModule(...)` call names a module by scope instead
        // of by path, so it is left alone.
        let mut instance_path = match values_iter.next() {
            Some(Expression::Call(call)) => {
                let mut path = InstancePath::from_root();
                path.child(extract_service_name(call)?);
                path
            }
            Some(expression) => self.resolve_script_relative(expression, source)?,
            None => InstancePath::from_root(),
        };

        values_iter.for_each(|expression| {
            if let Expression::String(x) = expression {
                instance_path.child(x.get_string_value().unwrap_or_default());
            }
        });

        if let Some(ref sourcemap) = self.cached_sourcemap {
            if let Some(file_path) = sourcemap.get_file_path(instance_path) {
                return self.to_require_path(&file_path, source);
            }
        };

        None
    }

    /// Resolves a `script`-relative import base into an absolute instance path.
    ///
    /// A package module written as `TS.import(script, script.Parent, "x")` means
    /// "the sibling `x` of my parent". That is only meaningful while the module sits
    /// where it was compiled; the bundler needs the absolute target so it can decide
    /// whether to inline it at all, and so an excluded module keeps a call that still
    /// resolves at runtime.
    ///
    /// The chain is read as `script` followed by any number of `.Parent` hops. Anything
    /// else (a `TS.getModule` call, an indexed expression) has no path interpretation and
    /// returns `None`.
    fn resolve_script_relative(
        &self,
        expression: &Expression,
        source: &Path,
    ) -> Option<InstancePath> {
        let mut parents = 0usize;
        let mut current = match expression {
            Expression::Identifier(identifier) => {
                if identifier.get_name() != "script" {
                    return None;
                }
                None
            }
            Expression::Field(field) => Some(field.as_ref()),
            _ => return None,
        };

        while let Some(field) = current {
            if field.get_field().get_name() != "Parent" {
                return None;
            }
            parents += 1;
            current = match field.get_prefix() {
                Prefix::Identifier(identifier) => {
                    if identifier.get_name() != "script" {
                        return None;
                    }
                    None
                }
                Prefix::Field(next) => Some(next.as_ref()),
                _ => return None,
            };
        }

        let sourcemap = self.cached_sourcemap.as_ref()?;
        let mut instance_path = sourcemap.get_absolute_instance_path(source)?;
        for _ in 0..parents {
            instance_path.parent();
        }

        Some(instance_path)
    }

    fn resolve_roblox_require(&self, call: &FunctionCall, source: &Path) -> Option<PathBuf> {
        if !is_bare_require_call(call) {
            return None;
        }
        parse_roblox(call, source)
            .ok()
            .flatten()
            .and_then(|x| self.to_require_path(&x, source))
    }

    /// Decide the path form to hand to `PathLocator::find_require_path`.
    ///
    /// A target already rooted at a configured source alias (for example
    /// `out/...` or `node_modules/...`) is returned unchanged. Two things depend
    /// on that form:
    ///
    /// - `find_require_path` joins an alias-prefixed path against the project
    ///   location, which is how the alias is meant to resolve.
    /// - `BundleOptions::is_excluded` matches its globs against this same value.
    ///   Those globs (`node_modules/**`, `out/test/**`) are written
    ///   project-relative, so rewriting the path first silently stops them from
    ///   matching and vendored code gets inlined and re-parsed instead of being
    ///   left as a runtime require.
    ///
    /// Anything not anchored by an alias (workspace packages reached through
    /// `../..`, for instance) is re-expressed relative to the requiring file,
    /// because `find_require_path` would otherwise read its first component as an
    /// unknown source name.
    fn to_require_path(&self, target: &Path, source: &Path) -> Option<PathBuf> {
        if let Some(root) = target.components().next() {
            if let Some(name) = root.as_os_str().to_str() {
                if self
                    .path_require_mode
                    .get_source(name, Path::new(""))
                    .is_some()
                {
                    return Some(target.to_path_buf());
                }
            }
        }

        rebase_to_source(target, source)
    }
}

/// Re-express a project-relative path so it can be handed to
/// `PathLocator::find_require_path`.
///
/// Both the Rojo sourcemap and `parse_roblox` hand back paths anchored at the
/// project location, but `find_require_path` resolves a leading `.` against the
/// *requiring file* (`source`) and treats anything else as an alias name. A
/// project-relative path fed in unchanged therefore resolves to the wrong place:
/// `packages/@internal/tether/out/init.luau` becomes "unknown source name
/// `packages`", and a path that walks above the project root gets appended to
/// the requiring file's directory.
///
/// The diff is computed component by component rather than with `pathdiff`.
/// Given two relative paths, `pathdiff::diff_paths` can emit a path like
/// `packages/../../../..` (it anchors on the target's first component instead of
/// counting the base's depth), which normalizes to the wrong directory. Walking
/// the components directly keeps both sides in the same project-relative
/// coordinate system, so the `..` count always matches the base's real depth.
fn rebase_to_source(target: &Path, source: &Path) -> Option<PathBuf> {
    let mut source_parent = source.to_path_buf();
    source_parent.pop();
    let base = utils::normalize_path(&source_parent);
    let target = utils::normalize_path(target);

    // An absolute target cannot be expressed relative to a relative base (and
    // vice versa): the two would resolve against different anchors.
    if base.has_root() != target.has_root() {
        return None;
    }

    let base_components: Vec<_> = base.components().collect();
    let target_components: Vec<_> = target.components().collect();

    let common = base_components
        .iter()
        .zip(target_components.iter())
        .take_while(|(left, right)| left == right)
        .count();

    let mut relative = PathBuf::new();
    for _ in common..base_components.len() {
        relative.push("..");
    }
    for component in &target_components[common..] {
        relative.push(component.as_os_str());
    }

    if relative.as_os_str().is_empty() {
        relative.push(".");
    }

    Some(PathBuf::from("./").join(relative))
}

/// Returns true only for a bare `require(...)` call.
///
/// `parse_roblox` treats a call's first argument as an instance path, so without
/// this guard any call whose first argument is a field expression -- for example
/// `options.spawnSound` or `applyPartVisual(part, original, options)` -- is
/// mis-read as a Roblox require. The bundler then tries to resolve a path built
/// out of the argument's field names (`...\spawnSound\options`) and fails.
///
/// The resolver runs before `is_require_call`, so it must screen the callee
/// itself. Shadowing is not considered here: a local named `require` is rare
/// enough that the structural check is the right trade for a resolver, and the
/// bundle rule still applies `is_require_call` (with its tracker) before
/// inlining.
fn is_bare_require_call(call: &FunctionCall) -> bool {
    if call.get_method().is_some() {
        return false;
    }

    match call.get_prefix() {
        Prefix::Identifier(identifier) => {
            identifier.get_name() == "require" && identifier.get_token().is_none()
        }
        _ => false,
    }
}

fn get_relative_parent_path(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) => {
            if parent == Path::new("") {
                Path::new(".")
            } else {
                parent
            }
        }
        None => Path::new(".."),
    }
}

fn extract_service_name(call: &FunctionCall) -> Option<String> {
    let field = call.get_method()?;

    let Prefix::Identifier(identifier) = call.get_prefix() else {
        return None;
    };

    if identifier.get_name() != "game" {
        return None;
    }

    if field.get_name() != "GetService" {
        return None;
    }

    let Arguments::Tuple(args) = call.get_arguments() else {
        return None;
    };

    let mut args_iter = args.iter_values();
    let first_arg = args_iter.next()?;
    if args_iter.next().is_some() {
        return None;
    }

    let Expression::String(service_name) = first_arg else {
        return None;
    };

    Some(
        service_name
            .get_string_value()
            .unwrap_or_default()
            .to_string(),
    )
}
