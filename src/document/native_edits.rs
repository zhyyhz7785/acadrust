//! Exact, scoped entity-graph edits without a whole-document snapshot.
//!
//! The initial coverage is deliberately limited to fully typed, in-memory
//! documents with valid symbol-table/header references. Opaque DWG imports,
//! unknown reference-bearing types, and ambiguous ownership are rejected
//! before mutation. The reference scanner is not an exhaustive audit of every
//! symbol-table/header field. This module is a contribution candidate, not a
//! complete replacement for general CAD editing or imported-drawing ERASE.

use super::{CadDocument, EntityCommon, EntityType, Handle, ObjectType, TableEntry};
use crate::objects::XRecordValue;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeEditError {
    Missing(Handle),
    Conflict(Handle),
    Unsupported(&'static str),
    HandleOverflow,
}

impl fmt::Display for NativeEditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(h) => write!(f, "missing native record {:X}", h.value()),
            Self::Conflict(h) => write!(f, "conflicting native record {:X}", h.value()),
            Self::Unsupported(reason) => write!(f, "native edit unsupported: {reason}"),
            Self::HandleOverflow => write!(f, "native handle space exhausted"),
        }
    }
}

impl std::error::Error for NativeEditError {}
pub type NativeEditResult<T> = std::result::Result<T, NativeEditError>;

/// The before and after values of one touched record. Absent values denote
/// insertion/removal, not an empty or default record.
#[derive(Debug, Clone)]
pub struct NativePatch<T> {
    pub handle: Handle,
    pub before: Option<T>,
    pub after: Option<T>,
}

impl<T> NativePatch<T> {
    fn values(&self, forward: bool) -> (Option<&T>, Option<&T>) {
        if forward {
            (self.before.as_ref(), self.after.as_ref())
        } else {
            (self.after.as_ref(), self.before.as_ref())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeMemberKind {
    Entity,
    Insert,
}

/// Positions are in the longer sequence; a deletion stores only removed
/// positions, and an append stores only the new suffix. History does not
/// retain a copy of every member of an affected block.
#[derive(Debug, Clone)]
pub struct NativeMemberPatch {
    pub owner: Handle,
    pub kind: NativeMemberKind,
    pub longer_len: usize,
    pub entries: Vec<(usize, Handle)>,
    pub add_forward: bool,
}

impl NativeMemberPatch {
    fn replay(&self, current: &[Handle], forward: bool) -> NativeEditResult<Vec<Handle>> {
        let add = forward == self.add_forward;
        let shorter = self.longer_len.checked_sub(self.entries.len())
            .ok_or(NativeEditError::Conflict(self.owner))?;
        let expected = if add { shorter } else { self.longer_len };
        if current.len() != expected {
            return Err(NativeEditError::Conflict(self.owner));
        }
        let mut positions = self.entries.iter().peekable();
        let mut result = Vec::with_capacity(if add { self.longer_len } else { shorter });
        if add {
            let removed: HashSet<_> = self.entries.iter().map(|(_, h)| *h).collect();
            if current.iter().any(|h| removed.contains(h)) {
                return Err(NativeEditError::Conflict(self.owner));
            }
            let mut retained = current.iter().copied();
            for index in 0..self.longer_len {
                if positions.peek().is_some_and(|(pos, _)| *pos == index) {
                    result.push(positions.next().expect("checked position").1);
                } else {
                    result.push(retained.next().ok_or(NativeEditError::Conflict(self.owner))?);
                }
            }
        } else {
            for (index, handle) in current.iter().copied().enumerate() {
                if positions.peek().is_some_and(|(pos, _)| *pos == index) {
                    if positions.next().expect("checked position").1 != handle {
                        return Err(NativeEditError::Conflict(self.owner));
                    }
                } else {
                    result.push(handle);
                }
            }
        }
        if positions.next().is_some() {
            return Err(NativeEditError::Conflict(self.owner));
        }
        Ok(result)
    }
}

/// A reusable exact in-memory change record. This is not a serialization
/// format. Apply it only to the same document, in history order. Replay
/// checks touched records and reference conflicts, and never swaps the
/// header, tables, allocator, path or other unrelated document state.
#[derive(Debug, Default, Clone)]
pub struct NativeChange {
    entities: Vec<NativePatch<Arc<EntityType>>>,
    objects: Vec<NativePatch<ObjectType>>,
    reactors: Vec<NativePatch<Vec<Handle>>>,
    dictionaries: Vec<NativePatch<Handle>>,
    members: Vec<NativeMemberPatch>,
    copies: Vec<(Handle, Handle)>,
    top_level_copies: Vec<(Handle, Handle)>,
    allocation_floor: u64,
}

impl NativeChange {
    pub fn entity_changes(&self) -> &[NativePatch<Arc<EntityType>>] { &self.entities }
    pub fn object_changes(&self) -> &[NativePatch<ObjectType>] { &self.objects }
    pub fn reactor_changes(&self) -> &[NativePatch<Vec<Handle>>] { &self.reactors }
    pub fn dictionary_changes(&self) -> &[NativePatch<Handle>] { &self.dictionaries }
    pub fn member_changes(&self) -> &[NativeMemberPatch] { &self.members }
    /// All reallocated identities, including nested attributes and objects.
    pub fn copied_handles(&self) -> &[(Handle, Handle)] { &self.copies }
    pub fn copied_entities(&self) -> &[(Handle, Handle)] { &self.top_level_copies }
    pub fn allocation_floor(&self) -> u64 { self.allocation_floor }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EdgeKind {
    Reference,
    Reactor,
    SideReactor,
    Group,
    Sort,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Edge {
    source: Handle,
    target: Handle,
    kind: EdgeKind,
}

#[derive(Default)]
struct Graph {
    identities: HashSet<Handle>,
    children: HashMap<Handle, Vec<Handle>>,
    edges: Vec<Edge>,
    top_members: HashMap<Handle, Handle>,
    shared_definitions: HashSet<Handle>,
    protected_table_refs: HashSet<Handle>,
    block_names: HashMap<Handle, String>,
    entity_dictionaries: HashMap<Handle, Handle>,
    nested_parent: HashMap<Handle, Handle>,
    floor: u64,
}

fn unsupported(reason: &'static str) -> NativeEditError {
    NativeEditError::Unsupported(reason)
}

impl Graph {
    fn reserve(&mut self, h: Handle) -> NativeEditResult<()> {
        if !h.is_null() {
            self.floor = self.floor.max(h.value().checked_add(1)
                .ok_or(NativeEditError::HandleOverflow)?);
        }
        Ok(())
    }

    fn identity(&mut self, h: Handle) -> NativeEditResult<()> {
        if h.is_null() || !self.identities.insert(h) {
            return Err(NativeEditError::Conflict(h));
        }
        self.reserve(h)
    }

    fn edge(&mut self, source: Handle, target: Handle, kind: EdgeKind) -> NativeEditResult<()> {
        if !target.is_null() {
            self.reserve(source)?;
            self.reserve(target)?;
            self.edges.push(Edge { source, target, kind });
        }
        Ok(())
    }

    fn references(&mut self, source: Handle, targets: impl IntoIterator<Item=Handle>) -> NativeEditResult<()> {
        for target in targets { self.edge(source, target, EdgeKind::Reference)?; }
        Ok(())
    }

    fn reactor_edges(&mut self, source: Handle, targets: &[Handle], kind: EdgeKind) -> NativeEditResult<()> {
        for target in targets { self.edge(source, *target, kind)?; }
        Ok(())
    }

    fn own(&mut self, owner: Handle, child: Handle) {
        if !owner.is_null() {
            self.children.entry(owner).or_default().push(child);
        }
    }

    fn common(&mut self, c: &EntityCommon) -> NativeEditResult<()> {
        if c.raw_record.is_some() || c.graphic_data.is_some() || c.has_ds_data
            || c.r12_metadata.is_some() || !c.extended_data.records().is_empty()
            || !c.extended_data.raw_dwg_eed.is_empty()
        {
            return Err(unsupported("opaque entity data or XDATA"));
        }
        self.references(c.handle, [c.owner_handle])?;
        self.references(c.handle, [
            c.layer_handle, c.linetype_handle, c.color_book_handle,
            c.full_visual_style_handle, c.face_visual_style_handle,
            c.edge_visual_style_handle, c.material_handle, c.plotstyle_handle,
            c.xdictionary_handle,
        ].into_iter().flatten())?;
        self.reactor_edges(c.handle, &c.reactors, EdgeKind::Reactor)
    }

    fn entity(&mut self, doc: &CadDocument, e: &EntityType) -> NativeEditResult<()> {
        let h = e.common().handle;
        self.identity(h)?;
        self.common(e.common())?;
        match e {
            EntityType::Point(_) | EntityType::Line(_) | EntityType::Circle(_)
            | EntityType::Arc(_) | EntityType::Ellipse(_) | EntityType::LwPolyline(_)
            | EntityType::Solid(_) | EntityType::Face3D(_) | EntityType::Ray(_)
            | EntityType::XLine(_) | EntityType::BlockEnd(_) => {}
            EntityType::Block(b) if b.xref_path.is_empty() => {}
            EntityType::Text(t) if !t.value.contains("%<") => {}
            EntityType::MText(t) if !t.is_annotative && !t.value.contains("%<") => {}
            EntityType::AttributeDefinition(a)
                if a.embedded_mtext.is_none() && !a.default_value.contains("%<") => {}
            EntityType::Insert(i) if i.view_rep_handle.is_none() => {
                let definition = doc.block_records.get(&i.block_name)
                    .ok_or(unsupported("missing block definition"))?;
                self.references(h, [definition.handle])?;
                for a in &i.attributes {
                    if a.embedded_mtext.is_some() || a.value.contains("%<")
                        || a.common.owner_handle != h {
                        return Err(unsupported("complex or invalid nested attribute"));
                    }
                    let child = a.common.handle;
                    self.identity(child)?;
                    self.nested_parent.insert(child, h);
                    self.own(h, child);
                    self.common(&a.common)?;
                    self.references(child, [a.attdef_handle])?;
                }
                if let Some(seqend) = i.seqend_handle {
                    self.identity(seqend)?;
                    self.nested_parent.insert(seqend, h);
                    self.own(h, seqend);
                }
            }
            _ => return Err(unsupported("entity reference coverage not implemented")),
        }
        Ok(())
    }

    fn object(&mut self, h: Handle, o: &ObjectType) -> NativeEditResult<()> {
        if o.intrinsic_handle() != h {
            return Err(NativeEditError::Conflict(h));
        }
        self.identity(h)?;
        let owner = match o {
            ObjectType::Dictionary(v) => v.owner,
            ObjectType::XRecord(v) => v.owner,
            ObjectType::Group(v) => v.owner,
            ObjectType::SortEntitiesTable(v) => v.owner_handle,
            ObjectType::Layout(v) => v.owner,
            ObjectType::DictionaryWithDefault(v) => v.owner,
            ObjectType::MLineStyle(v) => v.owner,
            ObjectType::PlaceHolder(v) => v.owner,
            ObjectType::MultiLeaderStyle(v) => v.owner_handle,
            ObjectType::TableStyle(v) => v.owner_handle,
            _ => return Err(unsupported("object reference coverage not implemented")),
        };
        self.own(owner, h);
        self.references(h, [owner])?;
        match o {
            ObjectType::Dictionary(d) => {
                self.references(h, d.entries.iter().map(|(_, h)| *h))?;
                self.references(h, d.xdictionary_handle)?;
                self.reactor_edges(h, &d.reactors, EdgeKind::Reactor)?;
            }
            ObjectType::XRecord(x) => {
                if !x.entries_complete || !x.raw_data.is_empty() || x.raw_dwg_data.is_some()
                    || x.preserve_object_reference_stream {
                    return Err(unsupported("opaque XRecord"));
                }
                self.references(h, x.xdictionary_handle)?;
                self.reactor_edges(h, &x.reactors, EdgeKind::Reactor)?;
                for e in &x.entries {
                    match &e.value {
                        XRecordValue::Handle(target) => self.references(h, [*target])?,
                        XRecordValue::Chunk(_) => return Err(unsupported("binary XRecord")),
                        XRecordValue::String(_) if matches!(e.code, 5 | 105 | 320..=369 | 390..=399 | 480..=481 | 1005) =>
                            return Err(unsupported("string-encoded XRecord handle")),
                        _ => {}
                    }
                }
                self.references(h, x.object_references.iter().map(|r| r.handle))?;
            }
            ObjectType::Group(g) => {
                for target in &g.entities { self.edge(h, *target, EdgeKind::Group)?; }
            }
            ObjectType::SortEntitiesTable(s) if s.raw_dxf_codes.is_none() => {
                self.references(h, [s.block_owner_handle])?;
                for entry in s.entries() {
                    self.reserve(entry.sort_handle)?;
                    self.edge(h, entry.entity_handle, EdgeKind::Sort)?;
                }
            }
            ObjectType::Layout(l) if l.raw_plot_settings_codes.is_none() => {
                self.references(h, [l.block_record, l.viewport, l.base_ucs, l.named_ucs,
                    l.plot_view_handle, l.visual_style_handle])?;
                self.references(h, l.viewports.iter().copied())?;
                self.references(h, l.xdictionary_handle)?;
                self.reactor_edges(h, &l.reactors, EdgeKind::Reactor)?;
            }
            ObjectType::DictionaryWithDefault(d) => {
                self.references(h, [d.default_handle])?;
                self.references(h, d.entries.iter().map(|(_, h)| *h))?;
            }
            ObjectType::MLineStyle(_) | ObjectType::PlaceHolder(_) => {}
            ObjectType::MultiLeaderStyle(s) => {
                self.references(h, [s.line_type_handle, s.arrowhead_handle,
                    s.text_style_handle, s.block_content_handle].into_iter().flatten())?;
            }
            ObjectType::TableStyle(s) if s.raw_dxf_codes.is_none()
                && s.modern_style.is_none() && s.modern_overrides.is_empty() => {
                self.references(h, [s.modern_cell_style_handle])?;
                self.references(h, [s.data_row_style.text_style_handle,
                    s.header_row_style.text_style_handle,
                    s.title_row_style.text_style_handle].into_iter().flatten())?;
            }
            _ => return Err(unsupported("object reference coverage not implemented")),
        }
        Ok(())
    }

    fn build(doc: &CadDocument) -> NativeEditResult<Self> {
        // Do not infer safety from an incomplete list of parsed references.
        // The first version supports only the closed typed graph below.
        if doc.dwg_source_version.is_some() || !doc.file_dependencies.is_empty()
            || !doc.vx_control_entries.is_empty() || !doc.eed_by_handle.is_empty()
            || !doc.object_xdata.is_empty() || !doc.raw_records.is_empty()
            || !doc.raw_record_owners.is_empty() || !doc.original_objects.is_empty()
            || doc.raw_acds_data.is_some() || !doc.raw_acds_fingerprint.is_empty()
            || doc.r12_header_raw.is_some() || !doc.r12_table_usage.is_empty()
            || !doc.r12_block_tags.is_empty() || !doc.block_entity_handles.is_empty()
            || !doc.dwg_data_store_handles.is_empty()
            || !doc.fields.is_empty() || !doc.block_visibility_params.is_empty()
            || !doc.block_representations.is_empty() || !doc.context_scales.is_empty()
            || !doc.dgn_ls_definitions.is_empty() || !doc.dgn_ls_components.is_empty()
            || !doc.view_rep_refs.is_empty() || !doc.section_view_reps.is_empty()
            || doc.section_view_style.is_some()
        {
            return Err(unsupported("document has opaque or unsupported side-channel data"));
        }
        let (table_max, protected_table_refs) =
            super::handle_usage::native_edit_table_protection(doc)
                .map_err(|_| unsupported("unqualified header or symbol-table references"))?;
        let floor = table_max.checked_add(1).ok_or(NativeEditError::HandleOverflow)?
            .max(doc.next_handle).max(doc.header.handle_seed).max(1);
        let mut g = Self { floor, protected_table_refs, ..Self::default() };
        macro_rules! table {
            ($t:expr) => {{
                let t = &$t;
                if !t.handle().is_null() { g.identity(t.handle())?; }
                for row in t.iter() {
                    if !row.handle().is_null() { g.identity(row.handle())?; }
                }
            }};
        }
        table!(doc.layers); table!(doc.line_types); table!(doc.text_styles);
        table!(doc.dim_styles); table!(doc.app_ids); table!(doc.views);
        table!(doc.vports); table!(doc.ucss); table!(doc.vx_table); table!(doc.block_records);
        g.shared_definitions.extend(g.identities.iter().copied());
        let mut markers = HashMap::new();
        for b in doc.block_records.iter() {
            if g.block_names.insert(b.handle, b.name.clone()).is_some() {
                return Err(NativeEditError::Conflict(b.handle));
            }
            for (handle, is_begin) in [(b.block_entity_handle, true), (b.block_end_handle, false)] {
                if handle.is_null() { continue; }
                if markers.insert(handle, (b.handle, is_begin)).is_some() {
                    return Err(NativeEditError::Conflict(handle));
                }
                if doc.get_entity(handle).is_none() {
                    // New documents can have structural handles without a
                    // corresponding flat BLOCK/ENDBLK record.
                    g.identity(handle)?;
                }
            }
            if b.flags.is_xref || b.flags.is_xref_overlay {
                return Err(unsupported("external block"));
            }
            for h in &b.entity_handles {
                g.reserve(*h)?;
                // Deleted membership tombstones are reserved but not live.
                if doc.get_entity(*h).is_some() && g.top_members.insert(*h, b.handle).is_some() {
                    return Err(NativeEditError::Conflict(*h));
                }
            }
            let mut seen_inserts = HashSet::new();
            for h in &b.insert_handles {
                g.reserve(*h)?;
                if let Some(e) = doc.get_entity(*h) {
                    if !seen_inserts.insert(*h)
                        || !matches!(e, EntityType::Insert(i) if doc.block_records
                            .get(&i.block_name).is_some_and(|definition| definition.handle == b.handle))
                    {
                        return Err(NativeEditError::Conflict(*h));
                    }
                }
            }
        }
        if doc.entity_index.len() != doc.entities.len() {
            return Err(unsupported("inconsistent entity index"));
        }
        for (index, entity) in doc.entities.iter().enumerate() {
            let h = entity.common().handle;
            if doc.entity_index.get(&h) != Some(&index) {
                return Err(NativeEditError::Conflict(h));
            }
            let membership_ok = match entity.as_ref() {
                EntityType::Block(_) => markers.get(&h) == Some(&(entity.common().owner_handle, true))
                    && !g.top_members.contains_key(&h),
                EntityType::BlockEnd(_) => markers.get(&h) == Some(&(entity.common().owner_handle, false))
                    && !g.top_members.contains_key(&h),
                _ => !markers.contains_key(&h)
                    && g.top_members.get(&h) == Some(&entity.common().owner_handle),
            };
            if !membership_ok { return Err(NativeEditError::Conflict(h)); }
            g.entity(doc, entity.as_ref())?;
        }
        for (h, object) in &doc.objects {
            g.object(*h, object)?;
            if matches!(object, ObjectType::MLineStyle(_) | ObjectType::MultiLeaderStyle(_)
                | ObjectType::TableStyle(_) | ObjectType::PlaceHolder(_)) {
                g.shared_definitions.insert(*h);
            }
        }
        // A tombstone may reserve an unused handle, but a live nested/object
        // identity may not masquerade as a direct block member.
        for b in doc.block_records.iter() {
            for h in b.entity_handles.iter().chain(&b.insert_handles) {
                if g.identities.contains(h) && doc.get_entity(*h).is_none() {
                    return Err(NativeEditError::Conflict(*h));
                }
            }
        }
        for (&owner, &dictionary) in &doc.xdic_by_handle {
            g.references(owner, [dictionary])?;
            if !g.identities.contains(&owner) || doc.object_owner(dictionary) != Some(owner)
                || !matches!(doc.objects.get(&dictionary), Some(ObjectType::Dictionary(_))) {
                return Err(unsupported("unresolved extension dictionary owner"));
            }
        }
        for owner in doc.reactors_by_handle.keys() {
            if !g.identities.contains(owner) {
                return Err(unsupported("unresolved side-channel reactor owner"));
            }
            g.reserve(*owner)?;
        }
        for (owner, reactor) in doc.iter_object_reactor_edges() {
            // A side-channel key may be a table record, not an ObjectType.
            if !g.identities.contains(&owner) {
                return Err(unsupported("unresolved side-channel reactor owner"));
            }
            g.edge(owner, reactor, EdgeKind::SideReactor)?;
        }
        // Check the typed extension dictionary edges, not just the fallback.
        for e in &doc.entities {
            let mut common = vec![e.common()];
            if let EntityType::Insert(i) = e.as_ref() {
                common.extend(i.attributes.iter().map(|a| &a.common));
            }
            for c in common {
                let side = doc.xdic_by_handle.get(&c.handle).copied();
                if c.xdictionary_handle.is_some() && side.is_some() && c.xdictionary_handle != side {
                    return Err(unsupported("inconsistent extension dictionary references"));
                }
                let dictionary = match c.xdictionary_handle.or(side) {
                    Some(h) => Some(h),
                    None => {
                        let mut owned = g.children.get(&c.handle).into_iter().flatten().copied()
                            .filter(|h| matches!(doc.objects.get(h), Some(ObjectType::Dictionary(_))));
                        let first = owned.next();
                        if owned.next().is_some() {
                            return Err(unsupported("ambiguous entity extension dictionary"));
                        }
                        first
                    }
                };
                if let Some(dictionary) = dictionary {
                    if doc.object_owner(dictionary) != Some(c.handle)
                        || !matches!(doc.objects.get(&dictionary), Some(ObjectType::Dictionary(_))) {
                        return Err(unsupported("invalid entity extension dictionary"));
                    }
                    g.entity_dictionaries.insert(c.handle, dictionary);
                }
            }
        }
        for edge in &g.edges {
            if !g.identities.contains(&edge.target) {
                return Err(unsupported("unresolved typed reference"));
            }
        }
        Ok(g)
    }

    fn closure(&self, doc: &CadDocument, roots: &[Handle]) -> NativeEditResult<HashSet<Handle>> {
        let mut result = HashSet::new();
        let mut pending = roots.to_vec();
        for h in roots {
            match doc.get_entity(*h) {
                None => return Err(NativeEditError::Missing(*h)),
                Some(EntityType::Block(_) | EntityType::BlockEnd(_)) =>
                    return Err(unsupported("structural block markers cannot be edited as entities")),
                Some(_) => {}
            }
        }
        while let Some(h) = pending.pop() {
            if !result.insert(h) { continue; }
            if let Some(object) = doc.objects.get(&h) {
                if !matches!(object, ObjectType::Dictionary(_) | ObjectType::XRecord(_)) {
                    return Err(unsupported("owned object requires a specialized edit"));
                }
            }
            if let Some(children) = self.children.get(&h) {
                pending.extend(children.iter().copied());
            }
        }
        Ok(result)
    }
}

fn entity_ids(entity: &EntityType, out: &mut HashSet<Handle>) {
    out.insert(entity.common().handle);
    if let EntityType::Insert(i) = entity {
        out.extend(i.attributes.iter().map(|a| a.common.handle));
        out.extend(i.seqend_handle);
    }
}

fn object_reactors_mut(object: &mut ObjectType) -> Option<&mut Vec<Handle>> {
    match object {
        ObjectType::Dictionary(d) => Some(&mut d.reactors),
        ObjectType::XRecord(x) => Some(&mut x.reactors),
        ObjectType::Layout(l) => Some(&mut l.reactors),
        _ => None,
    }
}

fn remap(h: Handle, mapping: &HashMap<Handle, Handle>) -> Handle {
    mapping.get(&h).copied().unwrap_or(h)
}

fn remap_common(c: &mut EntityCommon, mapping: &HashMap<Handle, Handle>) {
    c.handle = remap(c.handle, mapping);
    c.owner_handle = remap(c.owner_handle, mapping);
    // Keep every typed common reference consistent with the same map.
    for reference in [
        &mut c.layer_handle, &mut c.linetype_handle, &mut c.color_book_handle,
        &mut c.full_visual_style_handle, &mut c.face_visual_style_handle,
        &mut c.edge_visual_style_handle, &mut c.material_handle,
        &mut c.plotstyle_handle, &mut c.xdictionary_handle,
    ] {
        if let Some(h) = reference { *h = remap(*h, mapping); }
    }
    for h in &mut c.reactors { *h = remap(*h, mapping); }
    c.raw_record = None;
}

fn member_slice(block: &crate::tables::BlockRecord, kind: NativeMemberKind) -> &[Handle] {
    match kind { NativeMemberKind::Entity => &block.entity_handles, NativeMemberKind::Insert => &block.insert_handles }
}
fn member_slice_mut(block: &mut crate::tables::BlockRecord, kind: NativeMemberKind) -> &mut Vec<Handle> {
    match kind { NativeMemberKind::Entity => &mut block.entity_handles, NativeMemberKind::Insert => &mut block.insert_handles }
}

impl CadDocument {
    /// Remove selected flat entities and their supported owned subgraph,
    /// recording exact changes. Unsupported/opaque reference coverage is
    /// rejected, rather than silently deleting or keeping unknown dependents.
    /// This initial API is not a replacement for full CAD ERASE semantics.
    pub fn remove_entities(&mut self, handles: &[Handle]) -> NativeEditResult<NativeChange> {
        if handles.is_empty() { return Ok(NativeChange::default()); }
        let graph = Graph::build(self)?;
        let doomed = graph.closure(self, handles)?;
        if doomed.iter().any(|h| graph.protected_table_refs.contains(h)) {
            return Err(unsupported("header or symbol-table reference requires specialized removal"));
        }
        for e in &graph.edges {
            if doomed.contains(&e.target) && !doomed.contains(&e.source)
                && e.kind == EdgeKind::Reference {
                return Err(unsupported("incoming reference requires specialized removal"));
            }
        }
        let mut change = NativeChange { allocation_floor: graph.floor, ..NativeChange::default() };
        for entity in &self.entities {
            let h = entity.common().handle;
            if doomed.contains(&h) {
                change.entities.push(NativePatch { handle: h, before: Some(Arc::clone(entity)), after: None });
                continue;
            }
            let needs = entity.common().reactors.iter().any(|r| doomed.contains(r))
                || matches!(entity.as_ref(), EntityType::Insert(i) if i.attributes.iter()
                    .any(|a| a.common.reactors.iter().any(|r| doomed.contains(r))));
            if needs {
                let mut after = (**entity).clone();
                after.common_mut().reactors.retain(|r| !doomed.contains(r));
                if let EntityType::Insert(i) = &mut after {
                    for a in &mut i.attributes { a.common.reactors.retain(|r| !doomed.contains(r)); }
                }
                change.entities.push(NativePatch { handle: h, before: Some(Arc::clone(entity)), after: Some(Arc::new(after)) });
            }
        }
        let changed_object_sources: HashSet<_> = graph.edges.iter()
            .filter(|e| doomed.contains(&e.target)
                && matches!(e.kind, EdgeKind::Reactor | EdgeKind::Group | EdgeKind::Sort))
            .map(|e| e.source).collect();
        for (&h, object) in &self.objects {
            if doomed.contains(&h) {
                change.objects.push(NativePatch { handle: h, before: Some(object.clone()), after: None });
                continue;
            }
            let needs = changed_object_sources.contains(&h);
            if needs {
                let mut after = object.clone();
                if let Some(reactors) = object_reactors_mut(&mut after) {
                    reactors.retain(|r| !doomed.contains(r));
                }
                match &mut after {
                    ObjectType::Group(g) => g.entities.retain(|h| !doomed.contains(h)),
                    ObjectType::SortEntitiesTable(s) => {
                        let keep: Vec<_> = s.entries().filter(|e| !doomed.contains(&e.entity_handle))
                            .map(|e| (e.entity_handle, e.sort_handle)).collect();
                        s.clear();
                        for (entity, sort) in keep { s.add_entry(entity, sort); }
                    }
                    _ => {}
                }
                change.objects.push(NativePatch { handle: h, before: Some(object.clone()), after: Some(after) });
            }
        }
        for (&owner, reactors) in &self.reactors_by_handle {
            if doomed.contains(&owner) {
                change.reactors.push(NativePatch { handle: owner, before: Some(reactors.clone()), after: None });
            } else if reactors.iter().any(|h| doomed.contains(h)) {
                let after: Vec<_> = reactors.iter().copied().filter(|h| !doomed.contains(h)).collect();
                change.reactors.push(NativePatch { handle: owner, before: Some(reactors.clone()), after: Some(after) });
            }
        }
        for (&owner, &dictionary) in &self.xdic_by_handle {
            if doomed.contains(&owner) {
                change.dictionaries.push(NativePatch { handle: owner, before: Some(dictionary), after: None });
            }
        }
        for block in self.block_records.iter() {
            for kind in [NativeMemberKind::Entity, NativeMemberKind::Insert] {
                let members = member_slice(block, kind);
                let entries: Vec<_> = members.iter().copied().enumerate()
                    .filter(|(_, h)| doomed.contains(h)).collect();
                if !entries.is_empty() {
                    change.members.push(NativeMemberPatch {
                        owner: block.handle, kind, longer_len: members.len(), entries, add_forward: false,
                    });
                }
            }
        }
        self.apply_native_change_with_graph(&change, true, &graph)?;
        Ok(change)
    }

    /// Copy a supported same-document selection, in the supplied order.
    /// Shared definitions are not duplicated. Each INSERT attribute, SEQEND
    /// and supported owned dictionary/XRecord gets a new identity before any
    /// references are remapped. Group and explicit source draw-order entries
    /// are not copied; copies use their newly allocated default sort handles.
    /// Cross-document, unsupported association and opaque copy are rejected.
    pub fn copy_entities(&mut self, handles: &[Handle]) -> NativeEditResult<NativeChange> {
        self.copy_entities_with(handles, |_, _| Ok(()))
    }

    /// Like copy_entities, but apply a caller-supplied geometry transformation
    /// to each detached copy before committing. Identity, ownership and the
    /// supported reference graph must remain unchanged by the callback.
    /// Callback errors leave the drawing untouched.
    pub fn copy_entities_with(
        &mut self,
        handles: &[Handle],
        mut transform: impl FnMut(Handle, &mut EntityType) -> NativeEditResult<()>,
    ) -> NativeEditResult<NativeChange> {
        if handles.is_empty() { return Ok(NativeChange::default()); }
        let graph = Graph::build(self)?;
        let mut seen = HashSet::new();
        let roots: Vec<_> = handles.iter().copied().filter(|h| seen.insert(*h)).collect();
        let closure = graph.closure(self, &roots)?;
        for e in &graph.edges {
            if closure.contains(&e.source) && !closure.contains(&e.target) {
                if matches!(e.kind, EdgeKind::Reactor | EdgeKind::SideReactor)
                    || !(graph.shared_definitions.contains(&e.target)
                        || matches!(self.get_entity(e.target), Some(EntityType::AttributeDefinition(_))))
                    || graph.nested_parent.contains_key(&e.target) {
                    return Err(unsupported("copy requires explicit external reference policy"));
                }
            }
            if closure.contains(&e.target) && !closure.contains(&e.source)
                && !matches!(e.kind, EdgeKind::Group | EdgeKind::Sort) {
                return Err(unsupported("copy requires explicit incoming reference policy"));
            }
        }
        // Every owned dictionary entry must either belong to this closure
        // or the copy would need a separate shared-object policy.
        for h in &closure {
            if let Some(ObjectType::Dictionary(d)) = self.objects.get(h) {
                if d.entries.iter().any(|(_, target)| !closure.contains(target)
                    || self.object_owner(*target) != Some(*h)) {
                    return Err(unsupported("dictionary has shared or unresolved entries"));
                }
            }
        }
        let mut order = roots.clone();
        let mut other: Vec<_> = closure.iter().copied().filter(|h| !seen.contains(h)).collect();
        other.sort_by_key(|h| h.value());
        order.extend(other);
        // Header/table references also reserve their numeric slots even if
        // dangling; do not make a new copy accidentally satisfy such a link.
        let mut next = graph.floor;
        let mut mapping = HashMap::with_capacity(order.len());
        let mut change = NativeChange::default();
        for old in order {
            let new = Handle::new(next);
            next = next.checked_add(1).ok_or(NativeEditError::HandleOverflow)?;
            mapping.insert(old, new);
            change.copies.push((old, new));
        }
        let mut entity_appends: HashMap<Handle, Vec<Handle>> = HashMap::new();
        let mut insert_appends: HashMap<Handle, Vec<Handle>> = HashMap::new();
        for old in roots {
            let source = self.get_entity(old).ok_or(NativeEditError::Missing(old))?;
            let mut copy = source.clone();
            let dictionary = graph.entity_dictionaries.get(&old).copied();
            remap_common(copy.common_mut(), &mapping);
            copy.common_mut().xdictionary_handle = dictionary.map(|h| remap(h, &mapping));
            let new = copy.common().handle;
            if let EntityType::Insert(i) = &mut copy {
                for a in &mut i.attributes {
                    let dictionary = graph.entity_dictionaries.get(&a.common.handle).copied();
                    remap_common(&mut a.common, &mapping);
                    a.common.xdictionary_handle = dictionary.map(|h| remap(h, &mapping));
                    a.attdef_handle = remap(a.attdef_handle, &mapping);
                }
                if let Some(seqend) = i.seqend_handle {
                    i.seqend_handle = Some(remap(seqend, &mapping));
                } else if !i.attributes.is_empty() {
                    i.seqend_handle = Some(Handle::new(next));
                    next = next.checked_add(1).ok_or(NativeEditError::HandleOverflow)?;
                }
                let definition = self.block_records.get(&i.block_name)
                    .ok_or(unsupported("missing block definition"))?;
                insert_appends.entry(definition.handle).or_default().push(new);
            }
            let mut before_transform = Graph::default();
            before_transform.entity(self, &copy)?;
            // Capture the INSERT definition name too: graph equality alone
            // would not detect a name change that aliases the same record.
            let definition_before = match &copy {
                EntityType::Insert(i) => Some(i.block_name.clone()),
                _ => None,
            };
            let variant = std::mem::discriminant(&copy);
            transform(old, &mut copy)?;
            let mut after_transform = Graph::default();
            after_transform.entity(self, &copy)?;
            let definition_after = match &copy {
                EntityType::Insert(i) => Some(i.block_name.clone()),
                _ => None,
            };
            if copy.common().handle != new
                || variant != std::mem::discriminant(&copy)
                || definition_before != definition_after
                || before_transform.identities != after_transform.identities
                || before_transform.children != after_transform.children
                || before_transform.nested_parent != after_transform.nested_parent
                || before_transform.edges != after_transform.edges
            {
                return Err(unsupported("copy transformation changed the reference graph"));
            }
            entity_appends.entry(copy.common().owner_handle).or_default().push(new);
            change.top_level_copies.push((old, new));
            change.entities.push(NativePatch { handle: new, before: None, after: Some(Arc::new(copy)) });
        }
        for (&old, &new) in &mapping {
            if let Some(object) = self.objects.get(&old) {
                let mut copy = object.clone();
                match &mut copy {
                    ObjectType::Dictionary(d) => {
                        d.handle = new;
                        d.owner = remap(d.owner, &mapping);
                        for (_, h) in &mut d.entries { *h = remap(*h, &mapping); }
                        for h in &mut d.reactors { *h = remap(*h, &mapping); }
                        d.xdictionary_handle = d.xdictionary_handle.map(|h| remap(h, &mapping));
                    }
                    ObjectType::XRecord(x) => {
                        x.handle = new;
                        x.owner = remap(x.owner, &mapping);
                        for e in &mut x.entries {
                            if let XRecordValue::Handle(h) = &mut e.value { *h = remap(*h, &mapping); }
                        }
                        for r in &mut x.object_references { r.handle = remap(r.handle, &mapping); }
                        for h in &mut x.reactors { *h = remap(*h, &mapping); }
                        x.xdictionary_handle = x.xdictionary_handle.map(|h| remap(h, &mapping));
                    }
                    _ => return Err(unsupported("unsupported owned copy object")),
                }
                change.objects.push(NativePatch { handle: new, before: None, after: Some(copy) });
            }
            if let Some(reactors) = self.reactors_by_handle.get(&old) {
                change.reactors.push(NativePatch { handle: new, before: None,
                    after: Some(reactors.iter().map(|h| remap(*h, &mapping)).collect()) });
            }
            if let Some(dictionary) = self.xdic_by_handle.get(&old) {
                change.dictionaries.push(NativePatch { handle: new, before: None, after: Some(remap(*dictionary, &mapping)) });
            }
        }
        for block in self.block_records.iter() {
            for (kind, appends) in [(NativeMemberKind::Entity, &mut entity_appends),
                                    (NativeMemberKind::Insert, &mut insert_appends)] {
                if let Some(added) = appends.remove(&block.handle) {
                    let start = member_slice(block, kind).len();
                    let longer_len = start.checked_add(added.len()).ok_or(NativeEditError::HandleOverflow)?;
                    change.members.push(NativeMemberPatch { owner: block.handle, kind, longer_len,
                        entries: added.into_iter().enumerate().map(|(i, h)| (start+i, h)).collect(),
                        add_forward: true });
                }
            }
        }
        if !entity_appends.is_empty() || !insert_appends.is_empty() {
            return Err(unsupported("copy owner not found"));
        }
        change.allocation_floor = next;
        self.apply_native_change_with_graph(&change, true, &graph)?;
        Ok(change)
    }

    pub fn copy_entity(&mut self, handle: Handle) -> NativeEditResult<NativeChange> {
        self.copy_entities(&[handle])
    }

    /// Reverse a remove or copy change without allocating a new identity.
    pub fn restore_entities(&mut self, change: &NativeChange) -> NativeEditResult<()> {
        self.apply_native_change(change, false)
    }

    /// Reapply the same record; do not rerun the original delete/copy algorithm.
    pub fn redo_native_change(&mut self, change: &NativeChange) -> NativeEditResult<()> {
        self.apply_native_change(change, true)
    }

    fn apply_native_change(&mut self, change: &NativeChange, forward: bool) -> NativeEditResult<()> {
        if change.entities.is_empty() && change.objects.is_empty() && change.members.is_empty()
            && change.reactors.is_empty() && change.dictionaries.is_empty() { return Ok(()); }
        let graph = Graph::build(self)?;
        self.apply_native_change_with_graph(change, forward, &graph)
    }

    fn apply_native_change_with_graph(&mut self, change: &NativeChange, forward: bool, graph: &Graph) -> NativeEditResult<()> {
        let mut removed = HashSet::new();
        let mut added = HashSet::new();
        let mut touched_entities = HashSet::new();
        let mut touched_objects = HashSet::new();
        let mut touched_reactors = HashSet::new();
        for patch in &change.entities {
            let (before, after) = patch.values(forward);
            let current = self.get_entity_arc(patch.handle);
            match (before, current.as_ref()) {
                (Some(expected), Some(actual)) if Arc::ptr_eq(expected, actual) => {}
                (None, None) => {}
                _ => return Err(NativeEditError::Conflict(patch.handle)),
            }
            touched_entities.insert(patch.handle);
            if let Some(e) = before {
                if after.is_none() { entity_ids(e.as_ref(), &mut removed); }
            }
            if let Some(e) = after {
                if before.is_none() { entity_ids(e.as_ref(), &mut added); }
            }
        }
        for patch in &change.objects {
            let (before, after) = patch.values(forward);
            if self.objects.get(&patch.handle) != before { return Err(NativeEditError::Conflict(patch.handle)); }
            touched_objects.insert(patch.handle);
            if before.is_some() && after.is_none() { removed.insert(patch.handle); }
            if before.is_none() && after.is_some() { added.insert(patch.handle); }
        }
        for patch in &change.reactors {
            if self.reactors_by_handle.get(&patch.handle) != patch.values(forward).0 {
                return Err(NativeEditError::Conflict(patch.handle));
            }
            touched_reactors.insert(patch.handle);
        }
        for patch in &change.dictionaries {
            if self.xdic_by_handle.get(&patch.handle) != patch.values(forward).0 {
                return Err(NativeEditError::Conflict(patch.handle));
            }
        }
        for h in &added {
            if graph.identities.contains(h) || graph.protected_table_refs.contains(h) {
                return Err(NativeEditError::Conflict(*h));
            }
        }
        for h in &removed {
            if graph.protected_table_refs.contains(h) {
                return Err(NativeEditError::Conflict(*h));
            }
        }
        // The caller must use history order. Additionally refuse fresh
        // dependencies on records about to be removed, and unexpected
        // incoming references to a record that was absent.
        for e in &graph.edges {
            if added.contains(&e.target) && !added.contains(&e.source) {
                return Err(NativeEditError::Conflict(e.target));
            }
            if removed.contains(&e.target) && !removed.contains(&e.source) {
                let root = graph.nested_parent.get(&e.source).copied().unwrap_or(e.source);
                let covered = match e.kind {
                    EdgeKind::SideReactor => touched_reactors.contains(&e.source),
                    EdgeKind::Reactor => touched_entities.contains(&root) || touched_objects.contains(&e.source),
                    EdgeKind::Group | EdgeKind::Sort => touched_objects.contains(&e.source),
                    EdgeKind::Reference => false,
                };
                if !covered { return Err(NativeEditError::Conflict(e.target)); }
            }
        }
        // Validate outgoing references of restored/inserted/modified records
        // against the final identity set, without cloning the drawing. This
        // catches e.g. a definition removed between copy undo and redo.
        let mut staged = Graph::default();
        for patch in &change.entities {
            if let Some(value) = patch.values(forward).1 { staged.entity(self, value.as_ref())?; }
        }
        for patch in &change.objects {
            if let Some(value) = patch.values(forward).1 { staged.object(patch.handle, value)?; }
        }
        let live_after = |h: Handle| !removed.contains(&h)
            && (graph.identities.contains(&h) || added.contains(&h));
        for edge in &staged.edges {
            if !live_after(edge.target) { return Err(NativeEditError::Conflict(edge.target)); }
        }
        for patch in &change.reactors {
            if let Some(values) = patch.values(forward).1 {
                if !live_after(patch.handle) || values.iter().any(|h| !h.is_null() && !live_after(*h)) {
                    return Err(NativeEditError::Conflict(patch.handle));
                }
            }
        }
        for patch in &change.dictionaries {
            if let Some(target) = patch.values(forward).1 {
                let staged_owner = staged.children.get(&patch.handle).is_some_and(|children| children.contains(target));
                if !live_after(patch.handle) || !live_after(*target)
                    || !(staged_owner || self.object_owner(*target) == Some(patch.handle)) {
                    return Err(NativeEditError::Conflict(patch.handle));
                }
            }
        }
        let mut members = Vec::with_capacity(change.members.len());
        for patch in &change.members {
            let name = graph.block_names.get(&patch.owner).ok_or(NativeEditError::Missing(patch.owner))?;
            let block = self.block_records.get(name).filter(|b| b.handle == patch.owner)
                .ok_or(NativeEditError::Conflict(patch.owner))?;
            let result = patch.replay(member_slice(block, patch.kind), forward)?;
            members.push((name.clone(), patch.kind, result));
        }
        // All fallible logical checks are above. Replay only touched values.
        // The native Arc helpers preserve first-touch recording. No structure
        // clone/swap, no new handles on history replay.
        self.next_handle = self.next_handle.max(self.header.handle_seed)
            .max(change.allocation_floor).max(graph.floor);
        self.header.handle_seed = self.header.handle_seed.max(self.next_handle);
        for patch in &change.entities {
            match patch.values(forward) {
                (Some(_), None) => { self.remove_entity_arc(patch.handle).expect("preflight entity"); }
                (None, Some(after)) => { self.restore_entity_arc(Arc::clone(after)).expect("preflight new entity"); }
                (Some(_), Some(after)) => { self.replace_entity_arc(patch.handle, Arc::clone(after)).expect("preflight existing entity"); }
                (None, None) => {}
            }
        }
        for patch in &change.objects {
            if let Some(value) = patch.values(forward).1 { self.objects.insert(patch.handle, value.clone()); }
            else { self.objects.remove(&patch.handle); }
        }
        for patch in &change.reactors {
            if let Some(value) = patch.values(forward).1 { self.reactors_by_handle.insert(patch.handle, value.clone()); }
            else { self.reactors_by_handle.remove(&patch.handle); }
        }
        for patch in &change.dictionaries {
            if let Some(value) = patch.values(forward).1 { self.xdic_by_handle.insert(patch.handle, *value); }
            else { self.xdic_by_handle.remove(&patch.handle); }
        }
        for (name, kind, value) in members {
            let block = self.block_records.get_mut(&name).expect("preflight owner");
            *member_slice_mut(block, kind) = value;
        }
        Ok(())
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::{AttributeEntity, Block, BlockEnd, Insert, Line};
    use crate::tables::BlockRecord;
    use crate::types::{Color, DxfVersion, Vector3};

    fn line(doc: &mut CadDocument) -> Handle {
        doc.add_entity(EntityType::Line(Line::from_coords(
            0.0, 0.0, 0.0, 1.0, 2.0, 0.0,
        ))).unwrap()
    }

    #[test]
    fn remove_restores_owned_records_and_both_reactor_representations() {
        let mut doc = CadDocument::new();
        let target = line(&mut doc);
        let survivor = line(&mut doc);
        let record = doc.ensure_xrecord(target, "PAYLOAD");
        let dictionary = doc.extension_dictionary_handle(target).unwrap();
        doc.xrecord_mut(target, "PAYLOAD").unwrap().add_string(1, "kept");
        doc.get_entity_mut(survivor).unwrap().common_mut().reactors.push(target);
        let owner = doc.header.model_space_block_handle;
        doc.add_object_reactor(owner, target);
        let old_objects = doc.objects.len();
        let old_members = doc.block_records.get("*Model_Space").unwrap().entity_handles.clone();
        let old_seed = doc.header.handle_seed;
        let change = doc.remove_entities(&[target, target]).unwrap();
        assert_eq!(doc.entity_count(), 1);
        assert!(doc.get_entity(target).is_none());
        assert!(!doc.objects.contains_key(&record));
        assert!(!doc.objects.contains_key(&dictionary));
        assert_eq!(doc.objects.len(), old_objects - 2);
        assert!(doc.get_entity(survivor).unwrap().common().reactors.is_empty());
        assert!(doc.object_reactors(owner).is_empty());
        assert!(!doc.block_records.get("*Model_Space").unwrap().entity_handles.contains(&target));
        doc.layers.get_mut("0").unwrap().color = Color::RED;
        doc.source_path = Some("new-path.dwg".into());
        doc.restore_entities(&change).unwrap();
        assert_eq!(doc.objects.len(), old_objects);
        assert_eq!(doc.extension_dictionary_handle(target), Some(dictionary));
        assert_eq!(doc.xrecord(target, "PAYLOAD").unwrap().handle, record);
        assert_eq!(doc.get_entity(survivor).unwrap().common().reactors, vec![target]);
        assert_eq!(doc.object_reactors(owner), &[target]);
        assert_eq!(doc.block_records.get("*Model_Space").unwrap().entity_handles, old_members);
        assert_eq!(doc.layers.get("0").unwrap().color, Color::RED);
        assert_eq!(doc.source_path.as_deref(), Some("new-path.dwg"));
        assert!(doc.header.handle_seed >= old_seed);
        doc.redo_native_change(&change).unwrap();
        assert_eq!(doc.objects.len(), old_objects - 2);
        assert!(doc.get_entity(target).is_none());
        assert!(doc.allocate_handle().value() >= old_seed);
    }

    #[test]
    fn deep_copy_insert_allocates_nested_handles_and_owned_dictionary_graph() {
        let mut doc = CadDocument::new();
        let mut definition = BlockRecord::new("B");
        definition.handle = doc.allocate_handle();
        let definition_handle = definition.handle;
        doc.block_records.add(definition).unwrap();
        let mut begin = Block::new("B", Vector3::ZERO);
        begin.common.owner_handle = definition_handle;
        let begin_handle = doc.add_entity(EntityType::Block(begin)).unwrap();
        let mut end = BlockEnd::new();
        end.common.owner_handle = definition_handle;
        let end_handle = doc.add_entity(EntityType::BlockEnd(end)).unwrap();
        {
            let definition = doc.block_records.get_mut("B").unwrap();
            definition.block_entity_handle = begin_handle;
            definition.block_end_handle = end_handle;
        }
        let mut insert = Insert::new("B", Vector3::ZERO);
        insert.common.handle = doc.allocate_handle();
        let source = insert.common.handle;
        let mut attribute = AttributeEntity::simple("TAG", "VALUE");
        attribute.common.handle = doc.allocate_handle();
        let child = attribute.common.handle;
        attribute.common.owner_handle = source;
        insert.attributes.push(attribute);
        let sequence_end = doc.allocate_handle();
        insert.seqend_handle = Some(sequence_end);
        assert_eq!(doc.add_entity(EntityType::Insert(insert)).unwrap(), source);
        let source_record = doc.ensure_xrecord(source, "PAYLOAD");
        let source_dictionary = doc.extension_dictionary_handle(source).unwrap();
        doc.xrecord_mut(source, "PAYLOAD").unwrap().add_handle(340, child);
        let child_record = doc.ensure_xrecord(child, "ATTR_PAYLOAD");
        let child_dictionary = doc.extension_dictionary_handle(child).unwrap();
        let old_objects = doc.objects.len();

        let change = doc.copy_entities_with(&[source, source], |_, copy| {
            if let EntityType::Insert(i) = copy {
                i.insert_point = Vector3::new(1.0, 2.0, 3.0);
            }
            Ok(())
        }).unwrap();
        assert_eq!(change.copied_entities().len(), 1);
        let fresh = change.copied_entities()[0].1;
        let mapping: HashMap<_, _> = change.copied_handles().iter().copied().collect();
        let output = match doc.get_entity(fresh).unwrap() {
            EntityType::Insert(i) => i,
            _ => panic!("expected insert"),
        };
        assert_eq!(output.insert_point, Vector3::new(1.0, 2.0, 3.0));
        assert_ne!(fresh, source);
        assert_eq!(output.attributes[0].common.handle, mapping[&child]);
        assert_ne!(mapping[&child], child);
        assert_eq!(output.attributes[0].common.owner_handle, fresh);
        assert_eq!(output.seqend_handle, Some(mapping[&sequence_end]));
        assert_eq!(output.attributes[0].common.xdictionary_handle, Some(mapping[&child_dictionary]));
        assert_eq!(output.common.xdictionary_handle, Some(mapping[&source_dictionary]));
        let copied_record = doc.xrecord(fresh, "PAYLOAD").unwrap();
        assert_eq!(copied_record.handle, mapping[&source_record]);
        assert_eq!(copied_record.entries[0].value.as_handle(), Some(mapping[&child]));
        assert_eq!(doc.xrecord(source, "PAYLOAD").unwrap().entries[0].value.as_handle(), Some(child));
        assert_eq!(doc.xrecord(mapping[&child], "ATTR_PAYLOAD").unwrap().handle, mapping[&child_record]);
        assert_eq!(doc.objects.len(), old_objects + 4);
        let floor = doc.next_handle();
        doc.restore_entities(&change).unwrap();
        assert!(doc.get_entity(fresh).is_none());
        assert_eq!(doc.objects.len(), old_objects);
        assert!(!doc.block_records.get("B").unwrap().insert_handles.contains(&fresh));
        assert!(doc.next_handle() >= floor);
        doc.redo_native_change(&change).unwrap();
        assert!(doc.get_entity(fresh).is_some());
        assert_eq!(doc.objects.len(), old_objects + 4);
        assert_eq!(doc.block_records.get("B").unwrap().insert_handles.iter().filter(|h| **h == fresh).count(), 1);
        assert!(doc.next_handle() >= floor);
    }

    #[test]
    fn stale_replay_and_transform_failure_do_not_mutate_document() {
        let mut doc = CadDocument::new();
        let source = line(&mut doc);
        let seed = doc.next_handle();
        let error = doc.copy_entities_with(&[source], |_, copy| {
            copy.common_mut().handle = Handle::new(999);
            Ok(())
        });
        assert!(error.is_err());
        assert_eq!(doc.entity_count(), 1);
        assert_eq!(doc.next_handle(), seed);
        let change = doc.copy_entity(source).unwrap();
        let fresh = change.copied_entities()[0].1;
        let owner = doc.header.model_space_block_handle;
        doc.add_object_reactor(owner, fresh);
        assert!(doc.restore_entities(&change).is_err());
        assert_eq!(doc.entity_count(), 2);
        assert_eq!(doc.object_reactors(owner), &[fresh]);
    }

    #[test]
    fn opaque_import_is_rejected_without_partial_changes() {
        let mut doc = CadDocument::new();
        let target = line(&mut doc);
        doc.dwg_source_version = Some(DxfVersion::AC1032);
        let seed = doc.next_handle();
        assert!(matches!(doc.remove_entities(&[target]), Err(NativeEditError::Unsupported(_))));
        assert!(matches!(doc.copy_entity(target), Err(NativeEditError::Unsupported(_))));
        assert!(doc.get_entity(target).is_some());
        assert_eq!(doc.entity_count(), 1);
        assert_eq!(doc.next_handle(), seed);
    }

    #[test]
    fn ordered_member_patch_preserves_unremoved_members() {
        let ids: Vec<_> = (1..=5).map(Handle::new).collect();
        let patch = NativeMemberPatch {
            owner: Handle::new(10),
            kind: NativeMemberKind::Entity,
            longer_len: ids.len(),
            entries: vec![(1, ids[1]), (3, ids[3])],
            add_forward: false,
        };
        let shorter = patch.replay(&ids, true).unwrap();
        assert_eq!(shorter, vec![ids[0], ids[2], ids[4]]);
        assert_eq!(patch.replay(&shorter, false).unwrap(), ids);
        assert!(patch.replay(&[Handle::new(1)], false).is_err());
    }
}
