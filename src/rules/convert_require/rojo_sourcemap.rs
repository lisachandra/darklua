use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{utils, DarkluaError};

use super::instance_path::InstancePathComponent;
use super::InstancePath;

type NodeId = usize;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct RojoSourcemapNode {
    name: String,
    class_name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    file_paths: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    children: Vec<RojoSourcemapNode>,
    #[serde(skip)]
    id: NodeId,
    #[serde(skip)]
    parent_id: NodeId,
}

impl RojoSourcemapNode {
    fn initialize(mut self, relative_to: &Path) -> Self {
        let mut queue = vec![&mut self];
        let mut index = 0;

        while let Some(node) = queue.pop() {
            node.id = index;
            for file_path in &mut node.file_paths {
                *file_path = utils::normalize_path(relative_to.join(&file_path));
            }
            for child in &mut node.children {
                child.parent_id = index;
                queue.push(child);
            }
            index += 1;
        }

        self
    }

    fn id(&self) -> NodeId {
        self.id
    }

    fn parent_id(&self) -> NodeId {
        self.parent_id
    }

    fn iter(&self) -> impl Iterator<Item = &Self> {
        RojoSourcemapNodeIterator::new(self)
    }

    fn get_child(&self, id: NodeId) -> Option<&RojoSourcemapNode> {
        self.children.iter().find(|node| node.id == id)
    }

    fn get_descendant(&self, id: NodeId) -> Option<&RojoSourcemapNode> {
        self.iter().find(|node| node.id == id)
    }

    fn is_root(&self) -> bool {
        self.id == self.parent_id
    }
}

struct RojoSourcemapNodeIterator<'a> {
    queue: Vec<&'a RojoSourcemapNode>,
}

impl<'a> RojoSourcemapNodeIterator<'a> {
    fn new(root_node: &'a RojoSourcemapNode) -> Self {
        Self {
            queue: vec![root_node],
        }
    }
}

impl<'a> Iterator for RojoSourcemapNodeIterator<'a> {
    type Item = &'a RojoSourcemapNode;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(next_node) = self.queue.pop() {
            for child in &next_node.children {
                self.queue.push(child);
            }
            Some(next_node)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RojoSourcemap {
    root_node: RojoSourcemapNode,
    is_datamodel: bool,
}

impl RojoSourcemap {
    pub(crate) fn parse(
        content: &str,
        relative_to: impl AsRef<Path>,
    ) -> Result<Self, DarkluaError> {
        let root_node =
            serde_json::from_str::<RojoSourcemapNode>(content)?.initialize(relative_to.as_ref());

        let is_datamodel = root_node.class_name == "DataModel";
        Ok(Self {
            root_node,
            is_datamodel,
        })
    }

    pub(crate) fn exists(&self, path: &Path) -> bool {
        self.find_node(path).is_some()
    }

    /// Returns the absolute instance path of a file, from the DataModel root.
    ///
    /// Used to resolve `script`-relative import bases: the requiring file knows its
    /// own place in the tree, so `script.Parent` and the names that follow can be
    /// walked from there instead of being guessed at.
    pub(crate) fn get_absolute_instance_path(&self, file: &Path) -> Option<InstancePath> {
        let node = self.find_node(file)?;

        let mut names = Vec::new();
        let mut current = node;
        loop {
            if current.is_root() {
                break;
            }
            names.push(current.name.as_str());
            current = self.root_node.get_descendant(current.parent_id())?;
        }
        names.reverse();

        // The DataModel node is `game` itself, not a child of it, so its name is
        // skipped when the project describes a whole place.
        let mut instance_path = InstancePath::from_root();
        for name in names.iter().skip(if self.is_datamodel { 1 } else { 0 }) {
            if !name.is_empty() {
                instance_path.child(*name);
            }
        }

        Some(instance_path)
    }

    /// Returns true when the given file sits under a `node_modules` node.
    ///
    /// This is the only reliable way to identify a dependency: the same file can be
    /// reached through several paths because workspace links are junctions (on Windows)
    /// or symlinks. `packages/@lisachandra` and `../../Packages/rbxts/packages` are two
    /// names for one directory, so a glob written against the file path matches whichever
    /// spelling the resolver happened to produce and misses the other. The instance tree
    /// has no such ambiguity: a dependency is mounted under a `node_modules` folder, and
    /// project code is not.
    pub(crate) fn is_under_node_modules(&self, path: impl AsRef<Path>) -> bool {
        // The lookup is exact, and the path handed in comes from a resolver that
        // rebases and may leave `..` runs or a leading `.` behind, while the keys in
        // the sourcemap were normalized when it was parsed. Normalize before matching
        // or a genuine dependency is reported as project code and gets inlined.
        let normalized = utils::normalize_path(path.as_ref());
        let Some(node) = self.find_node(&normalized) else {
            return false;
        };

        self.hierarchy(node).iter().any(|id| {
            self.root_node
                .get_descendant(*id)
                .is_some_and(|ancestor| ancestor.name == "node_modules")
        })
    }

    pub(crate) fn get_instance_path(
        &self,
        from_file: impl AsRef<Path>,
        target_file: impl AsRef<Path>,
    ) -> Option<InstancePath> {
        let from_file = from_file.as_ref();
        let target_file = target_file.as_ref();

        let from_node = self.find_node(from_file)?;
        let target_node = self.find_node(target_file)?;

        let from_ancestors = self.hierarchy(from_node);
        let target_ancestors = self.hierarchy(target_node);

        let (parents, descendants, common_ancestor_id) = from_ancestors
            .iter()
            .enumerate()
            .find_map(|(index, ancestor_id)| {
                if let Some((target_index, common_ancestor_id)) = target_ancestors
                    .iter()
                    .enumerate()
                    .find(|(_, id)| *id == ancestor_id)
                {
                    Some((index, target_index, *common_ancestor_id))
                } else {
                    None
                }
            })
            .map(
                |(from_ancestor_split, target_ancestor_split, common_ancestor_id)| {
                    (
                        from_ancestors.split_at(from_ancestor_split).0,
                        target_ancestors.split_at(target_ancestor_split).0,
                        common_ancestor_id,
                    )
                },
            )?;

        let relative_path_length = parents.len().saturating_add(descendants.len());

        if !self.is_datamodel || relative_path_length <= target_ancestors.len() {
            log::trace!("  ⨽ use Roblox path from script instance");

            let mut instance_path = InstancePath::from_script();

            for _ in 0..parents.len() {
                instance_path.parent();
            }

            self.index_descendants(
                instance_path,
                self.root_node.get_descendant(common_ancestor_id)?,
                descendants.iter().rev(),
            )
        } else {
            log::trace!("  ⨽ use Roblox path from DataModel instance");

            self.index_descendants(
                InstancePath::from_root(),
                &self.root_node,
                target_ancestors.iter().rev().skip(1),
            )
        }
    }

    fn index_descendants<'a>(
        &self,
        mut instance_path: InstancePath,
        mut node: &RojoSourcemapNode,
        descendants: impl Iterator<Item = &'a usize>,
    ) -> Option<InstancePath> {
        for descendant_id in descendants {
            node = node.get_child(*descendant_id)?;
            instance_path.child(&node.name);
        }
        Some(instance_path)
    }

    /// returns the ids of each ancestor of the given node and itself
    fn hierarchy(&self, node: &RojoSourcemapNode) -> Vec<NodeId> {
        let mut ids = vec![node.id()];

        if node.is_root() {
            return ids;
        }

        let mut parent_id = node.parent_id();

        while let Some(parent) = self.root_node.get_descendant(parent_id) {
            ids.push(parent_id);
            if parent.is_root() {
                break;
            }
            parent_id = parent.parent_id();
        }

        ids
    }

    pub(crate) fn get_file_path(&self, instance_path: InstancePath) -> Option<PathBuf> {
        let mut node = &self.root_node;
        if self.is_datamodel {
            node = &self.root_node;
        }
        let target_node = instance_path.get_components().iter().try_fold(
            node,
            |node, component| match component {
                InstancePathComponent::Child(child_name) => {
                    node.children.iter().find(|x| &x.name == child_name)
                }
                InstancePathComponent::Parent => None,
            },
        );

        let target_node = target_node?;

        // A node can list several paths: an `init` module and the project file that
        // describes it, for instance. Only one of them is bundleable source, and
        // which one comes first is not guaranteed, so pick the script explicitly
        // instead of taking the first entry.
        target_node
            .file_paths
            .iter()
            .find(|path| {
                matches!(
                    path.extension().and_then(|extension| extension.to_str()),
                    Some("lua" | "luau")
                )
            })
            .or_else(|| target_node.file_paths.first())
            .cloned()
    }

    fn find_node(&self, path: &Path) -> Option<&RojoSourcemapNode> {
        self.root_node
            .iter()
            .find(|node| node.file_paths.iter().any(|file_path| file_path == path))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn new_sourcemap(content: &str) -> RojoSourcemap {
        RojoSourcemap::parse(content, "").expect("unable to parse sourcemap")
    }

    mod instance_paths {
        use super::*;

        fn script_path(components: &[&'static str]) -> InstancePath {
            components
                .iter()
                .fold(InstancePath::from_script(), |mut path, component| {
                    match *component {
                        "parent" => {
                            path.parent();
                        }
                        child_name => {
                            path.child(child_name);
                        }
                    }
                    path
                })
        }

        #[test]
        fn from_init_to_sibling_module() {
            let sourcemap = new_sourcemap(
                r#"{
                "name": "Project",
                "className": "ModuleScript",
                "filePaths": ["src/init.lua", "default.project.json"],
                "children": [
                    {
                        "name": "value",
                        "className": "ModuleScript",
                        "filePaths": ["src/value.lua"]
                    }
                ]
            }"#,
            );
            pretty_assertions::assert_eq!(
                sourcemap
                    .get_instance_path("src/init.lua", "src/value.lua")
                    .unwrap(),
                script_path(&["value"])
            );
        }

        #[test]
        fn from_sibling_to_sibling_module() {
            let sourcemap = new_sourcemap(
                r#"{
                "name": "Project",
                "className": "ModuleScript",
                "filePaths": ["src/init.lua", "default.project.json"],
                "children": [
                    {
                        "name": "main",
                        "className": "ModuleScript",
                        "filePaths": ["src/main.lua"]
                    },
                    {
                        "name": "value",
                        "className": "ModuleScript",
                        "filePaths": ["src/value.lua"]
                    }
                ]
            }"#,
            );
            pretty_assertions::assert_eq!(
                sourcemap
                    .get_instance_path("src/main.lua", "src/value.lua")
                    .unwrap(),
                script_path(&["parent", "value"])
            );
        }

        #[test]
        fn from_sibling_to_nested_sibling_module() {
            let sourcemap = new_sourcemap(
                r#"{
                "name": "Project",
                "className": "ModuleScript",
                "filePaths": ["src/init.lua", "default.project.json"],
                "children": [
                    {
                        "name": "main",
                        "className": "ModuleScript",
                        "filePaths": ["src/main.lua"]
                    },
                    {
                        "name": "Lib",
                        "className": "Folder",
                        "children": [
                            {
                                "name": "format",
                                "className": "ModuleScript",
                                "filePaths": ["src/Lib/format.lua"]
                            }
                        ]
                    }
                ]
            }"#,
            );
            pretty_assertions::assert_eq!(
                sourcemap
                    .get_instance_path("src/main.lua", "src/Lib/format.lua")
                    .unwrap(),
                script_path(&["parent", "Lib", "format"])
            );
        }

        #[test]
        fn from_child_require_parent() {
            let sourcemap = new_sourcemap(
                r#"{
                "name": "Project",
                "className": "ModuleScript",
                "filePaths": ["src/init.lua", "default.project.json"],
                "children": [
                    {
                        "name": "main",
                        "className": "ModuleScript",
                        "filePaths": ["src/main.lua"]
                    }
                ]
            }"#,
            );
            pretty_assertions::assert_eq!(
                sourcemap
                    .get_instance_path("src/main.lua", "src/init.lua")
                    .unwrap(),
                script_path(&["parent"])
            );
        }

        #[test]
        fn from_child_require_parent_nested() {
            let sourcemap = new_sourcemap(
                r#"{
                "name": "Project",
                "className": "ModuleScript",
                "filePaths": ["src/init.lua", "default.project.json"],
                "children": [
                    {
                        "name": "Sub",
                        "className": "ModuleScript",
                        "filePaths": ["src/Sub/init.lua"],
                        "children": [
                            {
                                "name": "test",
                                "className": "ModuleScript",
                                "filePaths": ["src/Sub/test.lua"]
                            }
                        ]
                    }
                ]
            }"#,
            );
            pretty_assertions::assert_eq!(
                sourcemap
                    .get_instance_path("src/Sub/test.lua", "src/Sub/init.lua")
                    .unwrap(),
                script_path(&["parent"])
            );
        }
    }

    mod dependency_detection {
        use super::*;

        /// A file mounted under a `node_modules` node is a dependency.
        ///
        /// This is the signal the bundler uses to leave a package as a runtime
        /// require (#851). It cannot be decided from the file path: workspace links
        /// are junctions on Windows and symlinks elsewhere, so one module is reachable
        /// under several spellings and no glob catches them all.
        const PROJECT: &str = r#"{
            "name": "Project",
            "className": "DataModel",
            "children": [
                {
                    "name": "ReplicatedStorage",
                    "className": "ReplicatedStorage",
                    "children": [
                        { "name": "src", "className": "Folder",
                          "children": [
                            { "name": "own", "className": "ModuleScript",
                              "filePaths": ["src/own.luau"] }
                          ] },
                        { "name": "node_modules", "className": "Folder",
                          "children": [
                            { "name": "lib", "className": "Folder",
                              "children": [
                                { "name": "init", "className": "ModuleScript",
                                  "filePaths": ["vendor/lib/init.luau"] }
                              ] }
                          ] }
                    ]
                }
            ]
        }"#;

        #[test]
        fn a_module_under_node_modules_is_a_dependency() {
            let sourcemap = new_sourcemap(PROJECT);

            assert!(sourcemap.is_under_node_modules("vendor/lib/init.luau"));
        }

        #[test]
        fn a_project_module_is_not_a_dependency() {
            let sourcemap = new_sourcemap(PROJECT);

            assert!(!sourcemap.is_under_node_modules("src/own.luau"));
        }

        /// The path arrives from a resolver that may leave a leading `.` or an interior
        /// `./`, while sourcemap keys are normalized when the map is parsed. Matching has
        /// to happen in one coordinate system or a real dependency reads as project code
        /// and gets inlined.
        #[test]
        fn a_dot_prefixed_spelling_of_a_dependency_is_still_a_dependency() {
            let sourcemap = new_sourcemap(PROJECT);

            assert!(sourcemap.is_under_node_modules("./vendor/lib/init.luau"));
            assert!(sourcemap.is_under_node_modules("vendor/./lib/init.luau"));
        }

        /// A file the sourcemap does not describe is not a dependency by default: an
        /// unknown path is more likely project code than a package, and treating it as a
        /// package would silently stop bundling real modules.
        #[test]
        fn an_unknown_file_is_not_a_dependency() {
            let sourcemap = new_sourcemap(PROJECT);

            assert!(!sourcemap.is_under_node_modules("src/unknown.luau"));
        }
    }
}
