//! Borrowed reverse index of observed native handle candidates.
//!
//! This is an inspection tool, not a complete deletion graph. Numeric
//! reservation candidates do not identify a reference role or establish
//! a permissible edit action. Missing or opaque carriers stay unqualified.

use super::{CadDocument, EntityCommon, EntityType, Handle, HandleReservationError, NativeDataCarrier, NativeDataPart};
use crate::io::dwg::eed_codec::EedHandleKind;
use crate::io::dwg::dwg_stream_readers::{object_reader::DwgObjectReader, merged_reader::RawHandleStreamInspection};
use crate::objects::ObjectType;
use crate::xdata::XDataValue;
use std::collections::HashMap;

/// Evidence categories may overlap: a modeled handle can also appear in
/// original EED or an explicit ownership edge. Do not count these as
/// distinct logical dependencies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NativeCandidateKind {
    ModeledNumeric,
    ModeledRole,
    Owner,
    Reactor,
    SideReactor,
    ExtensionDictionary,
    SideExtensionDictionary,
    LiveBlockMember,
    OriginalBlockMember,
    StructuredXdataHandle,
    RawEedApplication,
    RawEedLayer,
    RawEedHandle,
    RawHandleStream,
}

/// Which boundary vector a typed HATCH reference belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NativeHatchBoundaryList {
    Paths,
    InvalidMPolygonLoops,
    AnnotationContext,
}

/// Named MLEADER reference fields. Context and entity overrides are distinct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NativeMLeaderField {
    Style,
    LineType,
    Arrowhead,
    TextStyle,
    BlockContent,
    ContextTextStyle,
    ContextBlockContent,
    ContextScale,
    LineOverrideType,
    LineOverrideArrowhead,
}

/// Exact stored field location for the covered association families.
/// Indices refer to the current immutable drawing; they are not durable IDs
/// across edits. A role is evidence of a stored reference, not a deletion
/// policy, and does not qualify opaque payloads or inactive context values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NativeReferenceRole {
    HatchBoundary { list: NativeHatchBoundaryList, path: usize, slot: usize },
    DimensionAssociation,
    DimensionSnap { slot: usize, chain: usize, intersection: bool, index: usize },
    LeaderAnnotation,
    MLeaderField(NativeMLeaderField),
    MLeaderLine { root: usize, line: usize, field: NativeMLeaderField },
    MLeaderAttribute { index: usize },
    MLeaderArrowOverride { index: usize },
    GroupMember { index: usize },
    SortMember { index: usize },
    ProxyObjectId { index: usize },
    /// Original DWG handle-stream slot, without inferred field semantics.
    RawHandleSlot { bit_offset: i64, code: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeReferenceCandidate {
    /// None denotes grouped header/symbol-table numeric protection.
    /// For ModeledNumeric, an entity source is the enclosing flat record;
    /// other kinds can identify a nested record directly.
    pub source: Option<Handle>,
    pub kind: NativeCandidateKind,
    /// Present only for covered modeled field locations.
    pub role: Option<NativeReferenceRole>,
}

#[derive(Debug)]
pub struct NativeReferenceGap {
    pub source: Option<Handle>,
    pub unqualified_storage: bool,
    pub error: Option<HandleReservationError>,
}

/// A block with uncertain EED reference coverage. Binary application data
/// is intentionally not interpreted as a known list of CAD references.
#[derive(Debug, Clone, Copy)]
pub struct NativeEedReferenceGap {
    pub owner: Handle,
    pub carrier: NativeDataCarrier,
    pub application: Handle,
    pub undecoded: bool,
    pub unresolved_application: bool,
    pub binary: bool,
}

/// A retained Unknown object's independently framed handle stream.
/// None means original bytes or version are unavailable. Err means framing
/// or slot validation failed. Ok is still partial coverage of that object.
#[derive(Debug)]
pub struct NativeUnknownHandleStream {
    pub owner: Handle,
    pub result: Option<crate::error::Result<RawHandleStreamInspection>>,
}

/// An immutable, partial reference inventory tied to the borrowed drawing.
/// Public mutable CadDocument fields cannot invalidate it while the borrow
/// is live. It is not a cache shared across edits. Rebuild after mutation.
/// No returned candidate or absence of candidates certifies safe editing.
pub struct NativeReferenceCandidates<'a> {
    _document: &'a CadDocument,
    incoming: HashMap<Handle, Vec<NativeReferenceCandidate>>,
    gaps: Vec<NativeReferenceGap>,
    eed_gaps: Vec<NativeEedReferenceGap>,
    unknown_handle_streams: Vec<NativeUnknownHandleStream>,
    raw_records: usize,
    structured_binary_records: usize,
    unqualified_backing: Vec<(&'static str, usize)>,
    undecoded_eed: usize,
    unresolved_apps: usize,
    binary_eed: usize,
    opaque_records: usize,
}

impl<'a> NativeReferenceCandidates<'a> {
    fn new(document: &'a CadDocument) -> Self {
        Self {
            _document: document,
            incoming: HashMap::new(),
            gaps: Vec::new(),
            eed_gaps: Vec::new(),
            unknown_handle_streams: Vec::new(),
            raw_records: 0,
            structured_binary_records: 0,
            unqualified_backing: Vec::new(),
            undecoded_eed: 0,
            unresolved_apps: 0,
            binary_eed: 0,
            opaque_records: 0,
        }
    }

    fn add(&mut self, source: Option<Handle>, target: Handle, kind: NativeCandidateKind) {
        if !target.is_null() {
            self.incoming.entry(target).or_default().push(NativeReferenceCandidate { source, kind, role: None });
        }
    }

    fn add_role(&mut self, source: Option<Handle>, target: Handle, role: NativeReferenceRole) {
        if !target.is_null() {
            self.incoming.entry(target).or_default().push(NativeReferenceCandidate {
                source, kind: NativeCandidateKind::ModeledRole, role: Some(role),
            });
        }
    }

    fn common(&mut self, c: &EntityCommon) {
        let source = Some(c.handle);
        self.add(source, c.owner_handle, NativeCandidateKind::Owner);
        for &target in &c.reactors { self.add(source, target, NativeCandidateKind::Reactor); }
        if let Some(target) = c.xdictionary_handle {
            self.add(source, target, NativeCandidateKind::ExtensionDictionary);
        }
    }

    /// Observed incoming candidates for a non-null numeric target.
    pub fn incoming(&self, target: Handle) -> &[NativeReferenceCandidate] {
        self.incoming.get(&target).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn targets(&self) -> impl Iterator<Item = Handle> + '_ {
        self.incoming.keys().copied()
    }

    /// Known owner fields of entities/objects (including nested child records
    /// that this inspection visits). This is not a complete ownership closure.
    pub fn observed_children(&self, owner: Handle) -> impl Iterator<Item = Handle> + '_ {
        self.incoming(owner).iter()
            .filter(|edge| edge.kind == NativeCandidateKind::Owner)
            .filter_map(|edge| edge.source)
    }

    pub fn gaps(&self) -> &[NativeReferenceGap] { &self.gaps }
    pub fn eed_gaps(&self) -> &[NativeEedReferenceGap] { &self.eed_gaps }
    pub fn unknown_handle_streams(&self) -> &[NativeUnknownHandleStream] { &self.unknown_handle_streams }
    /// Count of observed raw carriers, not necessarily distinct records.
    pub fn raw_record_carriers(&self) -> usize { self.raw_records }
    pub fn structured_binary_records(&self) -> usize { self.structured_binary_records }
    pub fn unqualified_backing(&self) -> &[(&'static str, usize)] { &self.unqualified_backing }
    pub fn undecoded_eed_blocks(&self) -> usize { self.undecoded_eed }
    pub fn unresolved_app_blocks(&self) -> usize { self.unresolved_apps }
    pub fn binary_eed_blocks(&self) -> usize { self.binary_eed }
    pub fn opaque_records(&self) -> usize { self.opaque_records }
}

impl CadDocument {
    /// Build a read-only partial reverse-reference index for inspecting actual
    /// drawings. Uses the existing numeric handle walker and EED decoder.
    /// Missing coverage is reported rather than silently treated as absence.
    /// This does not change the strict edit/allocator preflight, nor supply
    /// a complete reverse-index maintenance contract for future transactions.
    pub fn native_reference_candidates(&self) -> NativeReferenceCandidates<'_> {
        let mut index = NativeReferenceCandidates::new(self);
        super::handle_usage::visit_reference_candidates(self, |source, candidates| {
            for &target in candidates.targets {
                index.add(source, target, NativeCandidateKind::ModeledNumeric);
            }
            for &(target, role) in candidates.roles {
                index.add_role(source, target, role);
            }
            if candidates.unqualified_storage || candidates.error.is_some() {
                index.gaps.push(NativeReferenceGap {
                    source,
                    unqualified_storage: candidates.unqualified_storage,
                    error: candidates.error,
                });
            }
        });
        for entity in &self.entities {
            index.common(entity.common());
            match entity.as_ref() {
                EntityType::Insert(i) => {
                    for a in &i.attributes { index.common(&a.common); }
                    if let Some(seqend) = i.seqend_handle {
                        index.add(Some(seqend), i.common.handle, NativeCandidateKind::Owner);
                    }
                }
                EntityType::PolyfaceMesh(m) => {
                    for v in &m.vertices { index.common(&v.common); }
                    for f in &m.faces { index.common(&f.common); }
                    if let Some(seqend) = m.seqend_handle {
                        index.add(Some(seqend), m.common.handle, NativeCandidateKind::Owner);
                    }
                }
                EntityType::PolygonMesh(m) => {
                    for v in &m.vertices { index.common(&v.common); }
                }
                _ => {}
            }
        }
        for &source in self.objects.keys() {
            if let Some(owner) = self.object_owner(source) {
                index.add(Some(source), owner, NativeCandidateKind::Owner);
            }
        }
        for (source, target) in self.iter_object_reactor_edges() {
            index.add(Some(source), target, NativeCandidateKind::SideReactor);
        }
        for (&source, &target) in &self.xdic_by_handle {
            index.add(Some(source), target, NativeCandidateKind::SideExtensionDictionary);
        }
        for block in self.block_records.iter() {
            for &target in &block.entity_handles {
                if self.get_entity(target).is_some() {
                    index.add(Some(block.handle), target, NativeCandidateKind::LiveBlockMember);
                }
            }
        }
        for (source, members) in self.iter_original_block_memberships() {
            for &target in members {
                index.add(Some(source), target, NativeCandidateKind::OriginalBlockMember);
            }
        }
        self.visit_native_data(|part| match part {
            NativeDataPart::StructuredXdata { owner, record, .. } => {
                if record.values.iter().any(|value| matches!(value, XDataValue::BinaryData(_))) {
                    index.structured_binary_records += 1;
                }
                for value in &record.values {
                    if let XDataValue::Handle(target) = value {
                        index.add(Some(owner), *target, NativeCandidateKind::StructuredXdataHandle);
                    }
                }
            }
            NativeDataPart::Eed(block) => {
                index.add(Some(block.owner), block.application, NativeCandidateKind::RawEedApplication);
                let undecoded = block.inspection.is_none();
                let unresolved_application = block.application_name.is_none();
                let binary = block.inspection.as_ref().is_some_and(|v| v.contains_binary_data);
                if unresolved_application { index.unresolved_apps += 1; }
                if undecoded || unresolved_application || binary {
                    index.eed_gaps.push(NativeEedReferenceGap {
                        owner: block.owner, carrier: block.carrier, application: block.application,
                        undecoded, unresolved_application, binary,
                    });
                }
                if let Some(inspection) = block.inspection {
                    if inspection.contains_binary_data { index.binary_eed += 1; }
                    for slot in inspection.handle_slots {
                        let kind = match slot.kind {
                            EedHandleKind::Layer => NativeCandidateKind::RawEedLayer,
                            EedHandleKind::Object => NativeCandidateKind::RawEedHandle,
                        };
                        index.add(Some(block.owner), slot.handle, kind);
                    }
                } else {
                    index.undecoded_eed += 1;
                }
            }
            NativeDataPart::OpaqueEntity { .. } => {
                index.opaque_records += 1;
            }
            NativeDataPart::OpaqueObject { owner, object } => {
                index.opaque_records += 1;
                if let ObjectType::Unknown {
                    raw_dwg_data, raw_dwg_handle_bits, raw_dwg_version, ..
                } = object {
                    let result = raw_dwg_data.as_deref().zip(*raw_dwg_version).map(|(bytes, version)| {
                        DwgObjectReader::inspect_raw_handle_stream(
                            bytes, *raw_dwg_handle_bits, version, owner.value(),
                        )
                    });
                    if let Some(Ok(inspection)) = &result {
                        for slot in &inspection.slots {
                            if slot.handle != 0 {
                                index.incoming.entry(Handle::new(slot.handle)).or_default()
                                    .push(NativeReferenceCandidate {
                                        source: Some(owner), kind: NativeCandidateKind::RawHandleStream,
                                        role: Some(NativeReferenceRole::RawHandleSlot {
                                            bit_offset: slot.bit_offset, code: slot.code,
                                        }),
                                    });
                            }
                        }
                    }
                    // Do not remove the opaque/gap status even on success.
                    index.unknown_handle_streams.push(NativeUnknownHandleStream { owner, result });
                }
            }
            NativeDataPart::OtherBacking { name, count } => {
                index.unqualified_backing.push((name, count));
            }
            NativeDataPart::RawEntity { .. } => { index.raw_records += 1; }
        });
        // Deterministic results per target and no repeated evidence of the
        // same kind/source/role tuple. Role indices preserve repeated targets
        // at distinct known fields. This is not an exhaustive field count.
        for candidates in index.incoming.values_mut() {
            candidates.sort_unstable_by_key(|edge| (edge.kind, edge.source.map(|h| h.value()), edge.role));
            candidates.dedup();
        }
        index
    }
}
