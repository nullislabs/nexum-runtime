//! Renders the resolved `nexum:host` package to a checked-in text snapshot, so
//! an added or retyped function signature lands as a reviewable diff.
//!
//! Only orderings the component ABI leaves free are sorted: record fields and
//! variant cases keep declaration order because their positions are the ABI.

use std::fmt::Write as _;
use std::path::Path;

use wit_parser::{
    Function, Handle, Resolve, Type, TypeDefKind, TypeId, TypeOwner, World, WorldItem,
};

/// Named in the failure message, so a red points at its own fix.
const REGENERATE: &str = "just wit-snapshot";

/// A WIT change that leaves the snapshot behind fails here. Set
/// `NEXUM_UPDATE_WIT_SNAPSHOT` (what `just wit-snapshot` does) to rewrite it.
#[test]
fn the_resolved_host_package_matches_the_snapshot() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = root.join("wit/nexum-host");
    let mut resolve = Resolve::new();
    resolve
        .push_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot parse {}: {e:?}", dir.display()));

    let rendered = render(&resolve);
    let path = root.join("wit/nexum-host.snapshot");
    if std::env::var_os("NEXUM_UPDATE_WIT_SNAPSHOT").is_some() {
        std::fs::write(&path, &rendered)
            .unwrap_or_else(|e| panic!("cannot write {}: {e}", path.display()));
        return;
    }

    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}; run `{REGENERATE}`", path.display()));
    if rendered == expected {
        return;
    }
    let (line, was, now) = first_difference(&expected, &rendered);
    panic!(
        "{} is stale at line {line}:\n  snapshot: {was}\n  wit tree: {now}\n\
         Regenerate it in this same change with `{REGENERATE}`.",
        path.display(),
    );
}

/// The one-based line number of the first disagreement and both sides of it.
fn first_difference(expected: &str, rendered: &str) -> (usize, String, String) {
    let was: Vec<&str> = expected.lines().collect();
    let now: Vec<&str> = rendered.lines().collect();
    let at = (0..was.len().max(now.len()))
        .find(|i| was.get(*i) != now.get(*i))
        .unwrap_or(0);
    let show = |lines: &[&str]| lines.get(at).copied().unwrap_or("<end of file>").to_owned();
    (at + 1, show(&was), show(&now))
}

fn render(resolve: &Resolve) -> String {
    let mut out = format!(
        "# The resolved nexum:host surface, rendered from wit/nexum-host.\n\
         # Regenerate with `{REGENERATE}`; never hand-edit.\n\n",
    );

    out.push_str(&by_name(resolve.packages.iter().map(|(_, pkg)| {
        (pkg.name.to_string(), format!("package {}\n", pkg.name))
    })));
    out.push_str(&by_name(resolve.interfaces.iter().filter_map(
        |(id, iface)| {
            let name = resolve.id_of(id)?;
            let mut items: Vec<String> = iface
                .types
                .iter()
                .map(|(name, id)| type_def(resolve, name, *id))
                .chain(iface.functions.values().map(|f| function(resolve, f)))
                .collect();
            items.sort();
            let block = format!("\ninterface {name} {{\n{}}}\n", indent(&items));
            Some((name, block))
        },
    )));
    out.push_str(&by_name(resolve.worlds.iter().map(|(_, w)| {
        let name = (w.package).map_or_else(|| w.name.clone(), |p| resolve.id_of_name(p, &w.name));
        let block = format!(
            "\nworld {name} {{\n{}}}\n",
            indent(&world_items(resolve, w))
        );
        (name, block)
    })));
    out
}

/// Sorted, because arena order follows the order the files were read.
fn by_name(blocks: impl Iterator<Item = (String, String)>) -> String {
    let mut blocks: Vec<(String, String)> = blocks.collect();
    blocks.sort();
    blocks.into_iter().map(|(_, block)| block).collect()
}

fn world_items(resolve: &Resolve, world: &World) -> Vec<String> {
    let mut items = Vec::new();
    for (direction, group) in [("import", &world.imports), ("export", &world.exports)] {
        let mut rendered: Vec<String> = group
            .iter()
            .map(|(key, item)| match item {
                WorldItem::Interface { id, .. } => {
                    let id = resolve.id_of(*id).unwrap_or_default();
                    format!("{direction} interface {id}")
                }
                WorldItem::Function(f) => format!("{direction} {}", function(resolve, f)),
                WorldItem::Type { id, .. } => {
                    let def = type_def(resolve, &String::from(key.clone()), *id);
                    format!("{direction} {def}")
                }
            })
            .collect();
        rendered.sort();
        items.append(&mut rendered);
    }
    items
}

fn function(resolve: &Resolve, f: &Function) -> String {
    let params: Vec<String> = f
        .params
        .iter()
        .map(|p| format!("{}: {}", p.name, type_ref(resolve, p.ty)))
        .collect();
    let asynchrony = if f.kind.is_async() { "async " } else { "" };
    let mut out = format!("{}: {asynchrony}func({})", f.name, params.join(", "));
    if let Some(result) = f.result {
        let _ = write!(out, " -> {}", type_ref(resolve, result));
    }
    out
}

/// Field and case order is the ABI, so it is reproduced, never sorted.
fn type_def(resolve: &Resolve, name: &str, id: TypeId) -> String {
    let block = |keyword: &str, entries: Vec<String>| {
        format!(
            "{keyword} {name} {{\n{}}}",
            entries
                .iter()
                .map(|e| format!("  {e},\n"))
                .collect::<String>()
        )
    };
    match &resolve.types[id].kind {
        TypeDefKind::Record(r) => block(
            "record",
            r.fields
                .iter()
                .map(|f| format!("{}: {}", f.name, type_ref(resolve, f.ty)))
                .collect(),
        ),
        TypeDefKind::Variant(v) => block(
            "variant",
            v.cases
                .iter()
                .map(|c| match c.ty {
                    Some(ty) => format!("{}({})", c.name, type_ref(resolve, ty)),
                    None => c.name.clone(),
                })
                .collect(),
        ),
        TypeDefKind::Enum(e) => block("enum", e.cases.iter().map(|c| c.name.clone()).collect()),
        TypeDefKind::Flags(f) => block("flags", f.flags.iter().map(|f| f.name.clone()).collect()),
        TypeDefKind::Resource => format!("resource {name}"),
        TypeDefKind::Type(ty) => format!("type {name} = {}", type_origin(resolve, *ty)),
        kind => format!("type {name} = {}", structure(resolve, kind)),
    }
}

/// A named type by its bare name: the item that declares it holds the origin.
fn type_ref(resolve: &Resolve, ty: Type) -> String {
    match ty {
        Type::Bool => "bool".to_owned(),
        Type::U8 => "u8".to_owned(),
        Type::U16 => "u16".to_owned(),
        Type::U32 => "u32".to_owned(),
        Type::U64 => "u64".to_owned(),
        Type::S8 => "s8".to_owned(),
        Type::S16 => "s16".to_owned(),
        Type::S32 => "s32".to_owned(),
        Type::S64 => "s64".to_owned(),
        Type::F32 => "f32".to_owned(),
        Type::F64 => "f64".to_owned(),
        Type::Char => "char".to_owned(),
        Type::String => "string".to_owned(),
        Type::ErrorContext => "error-context".to_owned(),
        Type::Id(id) => match &resolve.types[id].name {
            Some(name) => name.clone(),
            None => structure(resolve, &resolve.types[id].kind),
        },
    }
}

/// A named type qualified by the interface that owns it, so a `use` alias
/// records where it points rather than restating its own name.
fn type_origin(resolve: &Resolve, ty: Type) -> String {
    if let Type::Id(id) = ty
        && let Some(name) = &resolve.types[id].name
        && let TypeOwner::Interface(owner) = resolve.types[id].owner
        && let Some(owner) = resolve.id_of(owner)
    {
        return format!("{owner}/{name}");
    }
    type_ref(resolve, ty)
}

fn structure(resolve: &Resolve, kind: &TypeDefKind) -> String {
    let opt = |ty: Option<Type>| ty.map_or("_".to_owned(), |ty| type_ref(resolve, ty));
    match kind {
        TypeDefKind::Option(ty) => format!("option<{}>", type_ref(resolve, *ty)),
        TypeDefKind::List(ty) => format!("list<{}>", type_ref(resolve, *ty)),
        TypeDefKind::FixedLengthList(ty, n) => format!("list<{}, {n}>", type_ref(resolve, *ty)),
        TypeDefKind::Result(r) => format!("result<{}, {}>", opt(r.ok), opt(r.err)),
        TypeDefKind::Tuple(t) => {
            let types: Vec<String> = t.types.iter().map(|ty| type_ref(resolve, *ty)).collect();
            format!("tuple<{}>", types.join(", "))
        }
        TypeDefKind::Map(k, v) => {
            format!("map<{}, {}>", type_ref(resolve, *k), type_ref(resolve, *v))
        }
        TypeDefKind::Future(ty) => format!("future<{}>", opt(*ty)),
        TypeDefKind::Stream(ty) => format!("stream<{}>", opt(*ty)),
        // `as_str` is already `own` or `borrow` for a handle.
        TypeDefKind::Handle(Handle::Own(id) | Handle::Borrow(id)) => {
            format!("{}<{}>", kind.as_str(), type_ref(resolve, Type::Id(*id)))
        }
        TypeDefKind::Type(ty) => type_ref(resolve, *ty),
        kind => kind.as_str().to_owned(),
    }
}

fn indent(items: &[String]) -> String {
    items
        .iter()
        .flat_map(|item| item.lines().map(|line| format!("  {line}\n")))
        .collect()
}
