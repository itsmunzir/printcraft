//! Keeps only the XObjects a part's pages actually draw (Split / Extract Pages, #204).
//!
//! In a real book all pages share one `/Resources` dictionary, so every split part carried all
//! ~2,900 image XObjects while its 1,500 pages drew ~300 of them, and the part saved out at the
//! source's size. This pass follows each part's content streams (`Do`, form XObjects included)
//! and drops the `/XObject` entries no page names; the garbage-collecting rewrite on save then
//! drops the objects themselves.

use std::collections::{HashMap, HashSet};

use pdfcraft_cos::{Dict, Document, ObjRef, Object};

use crate::OrganizeError;

/// Form XObjects nested deeper than this are a broken file, not a reason to hang.
const MAX_FORM_DEPTH: u8 = 8;

/// The dictionary a set of drawn names belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum XKey {
    /// An indirect `/XObject` dictionary.
    XObject(ObjRef),
    /// An inline `/XObject` dictionary inside an indirect resources object.
    Resources(ObjRef),
    /// Everything inline on the page itself.
    Page(ObjRef),
}

/// Who owns the resources currently being walked, for the key drawn names are recorded under.
#[derive(Clone, Copy)]
enum Owner {
    /// An indirect resources object (a page's, or a form XObject's own).
    Resources(ObjRef),
    /// Resources inline on the page.
    Page(ObjRef),
}

struct Pruner<'a> {
    doc: &'a Document,
    used: HashMap<XKey, HashSet<Vec<u8>>>,
}

impl Pruner<'_> {
    /// The `/XObject` dictionary of `resources`, with the key its drawn names belong to.
    fn xobjects(&self, resources: &Dict, owner: Owner) -> Option<(Dict, XKey)> {
        let x = resources.get(b"XObject")?;
        if let Some(r) = x.as_ref() {
            return Some((self.doc.resolve(x).as_dict()?.clone(), XKey::XObject(r)));
        }
        let key = match owner {
            Owner::Resources(r) => XKey::Resources(r),
            Owner::Page(p) => XKey::Page(p),
        };
        Some((self.doc.resolve(x).as_dict()?.clone(), key))
    }

    /// Record every XObject name `data` draws with `Do`, descending into form XObjects.
    fn content(&mut self, data: &[u8], resources: &Dict, owner: Owner, depth: u8) {
        if depth > MAX_FORM_DEPTH {
            return;
        }
        let Some((xobjects, key)) = self.xobjects(resources, owner) else {
            return;
        };
        for op in pdfcraft_content::parse(data).ops {
            if op.op.as_slice() != b"Do" {
                continue;
            }
            let Some(name) = op.name(0) else { continue };
            self.used.entry(key).or_default().insert(name.to_vec());
            let Some(r) = xobjects.get(name).and_then(Object::as_ref) else { continue };
            let Object::Stream(s) = &*self.doc.get(r) else { continue };
            if s.dict.name(b"Subtype") != Some(b"Form") {
                continue;
            }
            let (inner, inner_owner) = match s.dict.get(b"Resources") {
                Some(res) => match res.as_ref() {
                    Some(rr) => match self.doc.get(rr).as_dict().cloned() {
                        Some(d) => (d, Owner::Resources(rr)),
                        None => continue,
                    },
                    None => match self.doc.resolve(res).as_dict().cloned() {
                        // The form carries its resources inline: filter them on the form itself.
                        Some(d) => (d, Owner::Resources(r)),
                        None => continue,
                    },
                },
                // A form without its own resources draws against the ones it was invoked with.
                None => (resources.clone(), owner),
            };
            self.content(&s.decoded().unwrap_or_default(), &inner, inner_owner, depth + 1);
        }
    }
}

/// The page's content, concatenated (`/Contents` may be one stream or an array).
fn content_bytes(doc: &Document, contents: &Object) -> Vec<u8> {
    match &*doc.resolve(contents) {
        Object::Stream(s) => s.decoded().unwrap_or_default(),
        Object::Array(a) => a
            .iter()
            .flat_map(|c| match &*doc.resolve(c) {
                Object::Stream(s) => {
                    let mut v = s.decoded().unwrap_or_default();
                    v.push(b'\n');
                    v
                }
                _ => Vec::new(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The resources of a page (as `walk` reports them) with their owner.
fn resolve_resources(doc: &Document, resources: &Object, page: ObjRef) -> Option<(Dict, Owner)> {
    match resources {
        Object::Ref(r) => doc.get(*r).as_dict().cloned().map(|d| (d, Owner::Resources(*r))),
        Object::Dict(d) => Some((d.clone(), Owner::Page(page))),
        _ => None,
    }
}

/// Drop the entries `names` never draws. Returns how many were removed.
fn drop_unused(xobjects: &mut Dict, names: &HashSet<Vec<u8>>) -> usize {
    let unused: Vec<Vec<u8>> = xobjects.iter().map(|(k, _)| k.clone()).filter(|k| !names.contains(k)).collect();
    for k in &unused {
        xobjects.remove(k);
    }
    unused.len()
}

/// Filter the dictionary `key` names to `names`.
fn filter_at(doc: &mut Document, key: XKey, names: &HashSet<Vec<u8>>) -> Result<usize, OrganizeError> {
    match key {
        XKey::XObject(r) => {
            let mut removed = 0;
            doc.update_dict(r, |d| removed = drop_unused(d, names))?;
            Ok(removed)
        }
        XKey::Resources(r) => {
            let Some(d) = doc.get(r).as_dict().cloned() else { return Ok(0) };
            let Some(x) = d.get(b"XObject") else { return Ok(0) };
            if x.as_ref().is_some() {
                return Ok(0); // recorded as XObject(r) instead
            }
            let Some(mut xd) = doc.resolve(x).as_dict().cloned() else { return Ok(0) };
            let removed = drop_unused(&mut xd, names);
            if removed > 0 {
                doc.update_dict(r, |d| d.set(b"XObject".to_vec(), Object::Dict(xd.clone())))?;
            }
            Ok(removed)
        }
        XKey::Page(p) => {
            let Some(pd) = doc.get(p).as_dict().cloned() else { return Ok(0) };
            let Some(res) = pd.get(b"Resources") else { return Ok(0) };
            if res.as_ref().is_some() {
                return Ok(0); // recorded as Resources(r) instead
            }
            let Some(mut rd) = doc.resolve(res).as_dict().cloned() else { return Ok(0) };
            let Some(x) = rd.get(b"XObject") else { return Ok(0) };
            if x.as_ref().is_some() {
                return Ok(0);
            }
            let Some(mut xd) = doc.resolve(x).as_dict().cloned() else { return Ok(0) };
            let removed = drop_unused(&mut xd, names);
            if removed > 0 {
                rd.set(b"XObject".to_vec(), Object::Dict(xd));
                doc.update_dict(p, |d| d.set(b"Resources".to_vec(), Object::Dict(rd.clone())))?;
            }
            Ok(removed)
        }
    }
}

/// Drop the page-resource entries the document's pages never draw. Returns how many were removed.
pub(crate) fn prune_unused_xobjects(doc: &mut Document) -> Result<usize, OrganizeError> {
    let pages = crate::walk(doc)?;
    let mut pruner = Pruner { doc, used: HashMap::new() };
    for (page, attrs) in &pages {
        let Some(res) = attrs.get(b"Resources") else { continue };
        let Some(pd) = pruner.doc.get(*page).as_dict().cloned() else { continue };
        let Some(contents) = pd.get(b"Contents").cloned() else { continue };
        let data = content_bytes(pruner.doc, &contents);
        if data.is_empty() {
            continue;
        }
        let Some((res_dict, owner)) = resolve_resources(pruner.doc, res, *page) else { continue };
        pruner.content(&data, &res_dict, owner, 0);
    }
    let used = pruner.used;
    let mut removed = 0;
    for (key, names) in used {
        removed += filter_at(doc, key, &names)?;
    }
    Ok(removed)
}
