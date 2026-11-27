//! Read-only native encoding and extended-data inventory for graph-edit research.
//!
//! This inventories actual storage carriers, not just decoded semantic records.
//! It is deliberately non-authoritative: unknown/raw backing may contain more
//! references than this visitor exposes. Never use an empty edge list as proof
//! that an imported drawing can be edited safely.

use super::{CadDocument, EntityCommon, EntityType, Handle, ObjectType};
use crate::entities::RawRecord;
use crate::io::dwg::eed_codec::{inspect_eed_payload, EedPayloadInspection};
use crate::types::DxfVersion;
use crate::xdata::{ExtendedData, ExtendedDataRecord};
use std::collections::HashMap;

/// Location of original encoded data. Distinct carriers can represent the same
/// logical record; their counts must not be added as unique entity counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeDataCarrier {
    Entity,
    NestedAttribute,
    NestedMeshRecord,
    ObjectStructured,
    NonEntityRaw,
    DebugRawRecord,
}

/// An original DWG EED block and its inspection result. The application name
/// is absent if its APPID cannot be resolved uniquely. Decoding can succeed
/// even if an APPID or layer is missing; the original numeric slots are kept.
#[derive(Debug)]
pub struct NativeEedBlock<'a> {
    pub owner: Handle,
    pub carrier: NativeDataCarrier,
    pub application: Handle,
    pub application_name: Option<&'a str>,
    pub bytes: &'a [u8],
    pub inspection: Option<EedPayloadInspection>,
}

/// Read-only inventory of encoding carriers. This is not a full CAD reference
/// audit: it does not parse arbitrary object bytes or application binary data,
/// and does not infer a deletion, copy or remapping policy.
#[derive(Debug)]
pub enum NativeDataPart<'a> {
    RawEntity {
        owner: Handle,
        carrier: NativeDataCarrier,
        raw: &'a RawRecord,
    },
    StructuredXdata {
        owner: Handle,
        carrier: NativeDataCarrier,
        record: &'a ExtendedDataRecord,
    },
    Eed(NativeEedBlock<'a>),
    OpaqueEntity {
        owner: Handle,
        type_name: &'static str,
    },
    OpaqueObject {
        owner: Handle,
        object: &'a ObjectType,
    },
    /// A native side-channel whose references are not qualified by this
    /// inventory. Count is the number of entries, not a byte count.
    OtherBacking {
        name: &'static str,
        count: usize,
    },
}

struct Inventory<'a, F> {
    applications: HashMap<u64, Option<&'a str>>,
    layers: HashMap<u64, Option<&'a str>>,
    wide: bool,
    visitor: F,
}

fn index_name<'a>(map: &mut HashMap<u64, Option<&'a str>>, handle: Handle, name: &'a str) {
    if !handle.is_null() {
        map.entry(handle.value()).and_modify(|slot| *slot = None).or_insert(Some(name));
    }
}

impl<'a, F: FnMut(NativeDataPart<'a>)> Inventory<'a, F> {
    fn raw(&mut self, owner: Handle, carrier: NativeDataCarrier, app: u64, bytes: &'a [u8]) {
        let application_name = self.applications.get(&app).copied().flatten();
        let inspection = inspect_eed_payload(bytes, self.wide, |h| {
            self.layers.get(&h).copied().flatten().map(str::to_owned)
        });
        (self.visitor)(NativeDataPart::Eed(NativeEedBlock {
            owner, carrier, application: Handle::new(app), application_name, bytes, inspection,
        }));
    }

    fn extended(&mut self, owner: Handle, carrier: NativeDataCarrier, data: &'a ExtendedData) {
        for record in data.records() {
            (self.visitor)(NativeDataPart::StructuredXdata { owner, carrier, record });
        }
        for (app, bytes) in &data.raw_dwg_eed {
            self.raw(owner, carrier, *app, bytes);
        }
    }

    fn common(&mut self, common: &'a EntityCommon, carrier: NativeDataCarrier) {
        if let Some(raw) = &common.raw_record {
            (self.visitor)(NativeDataPart::RawEntity { owner: common.handle, carrier, raw });
        }
        self.extended(common.handle, carrier, &common.extended_data);
    }

    fn backing(&mut self, name: &'static str, count: usize) {
        if count != 0 {
            (self.visitor)(NativeDataPart::OtherBacking { name, count });
        }
    }
}

impl CadDocument {
    /// Visit stored original entity bytes, structured extended data, original
    /// EED blocks and explicit opaque/side-channel carriers without editing.
    ///
    /// The visitor gets each carrier as stored, including both structured
    /// and raw EED for the same owner/application. It must not infer that
    /// those are independent logical references. Includes top-level entities,
    /// INSERT attributes and typed polyface/polygon-mesh child records;
    /// other compound/opaque payloads are not exhaustively decoded here.
    /// APPID/layer lookup is built once, so inspecting EED does not repeatedly
    /// traverse symbol tables. No data or document-wide snapshots are cloned.
    pub fn visit_native_data<'a>(&'a self, visitor: impl FnMut(NativeDataPart<'a>)) {
        let mut applications = HashMap::new();
        let mut layers = HashMap::new();
        for row in self.app_ids.iter() {
            index_name(&mut applications, row.handle, &row.name);
        }
        for row in self.layers.iter() {
            index_name(&mut layers, row.handle, &row.name);
        }
        let mut inventory = Inventory {
            applications,
            layers,
            wide: self.dwg_source_version.unwrap_or(self.version) >= DxfVersion::AC1021,
            visitor,
        };
        // Borrow self, not the inventory, while invoking its mutable visitor.
        for entity in &self.entities {
            inventory.common(entity.common(), NativeDataCarrier::Entity);
            match entity.as_ref() {
                EntityType::Insert(i) => {
                    for a in &i.attributes {
                        inventory.common(&a.common, NativeDataCarrier::NestedAttribute);
                    }
                }
                EntityType::PolyfaceMesh(m) => {
                    for v in &m.vertices {
                        inventory.common(&v.common, NativeDataCarrier::NestedMeshRecord);
                    }
                    for f in &m.faces {
                        inventory.common(&f.common, NativeDataCarrier::NestedMeshRecord);
                    }
                }
                EntityType::PolygonMesh(m) => {
                    for v in &m.vertices {
                        inventory.common(&v.common, NativeDataCarrier::NestedMeshRecord);
                    }
                }
                EntityType::Unknown(_) | EntityType::Extended(_) => {
                    (inventory.visitor)(NativeDataPart::OpaqueEntity {
                        owner: entity.common().handle,
                        type_name: if matches!(entity.as_ref(), EntityType::Unknown(_)) {
                            "Unknown"
                        } else {
                            "Extended"
                        },
                    });
                }
                _ => {}
            }
        }
        for (&owner, data) in &self.object_xdata {
            inventory.extended(owner, NativeDataCarrier::ObjectStructured, data);
        }
        for (owner, application, bytes) in self.iter_non_entity_raw_eed() {
            inventory.raw(owner, NativeDataCarrier::NonEntityRaw, application.value(), bytes);
        }
        for (owner, _, raw) in self.iter_debug_raw_records() {
            (inventory.visitor)(NativeDataPart::RawEntity {
                owner, carrier: NativeDataCarrier::DebugRawRecord, raw,
            });
        }
        for (&owner, object) in &self.objects {
            if matches!(object, ObjectType::Unknown { .. } | ObjectType::ProxyObject(_)) {
                (inventory.visitor)(NativeDataPart::OpaqueObject { owner, object });
            }
        }
        inventory.backing("original_objects", self.original_objects.len());
        inventory.backing("raw_record_owners", self.raw_record_owners.len());
        inventory.backing("block_entity_handles", self.block_entity_handles.len());
        inventory.backing("r12_header_raw", usize::from(self.r12_header_raw.is_some()));
        inventory.backing("r12_table_usage", self.r12_table_usage.len());
        inventory.backing("r12_block_tags", self.r12_block_tags.len());
        inventory.backing("raw_acds_data", usize::from(self.raw_acds_data.is_some()));
        inventory.backing("raw_acds_fingerprint", self.raw_acds_fingerprint.len());
        inventory.backing("dwg_data_store_handles", self.dwg_data_store_handles.len());
        inventory.backing("section_view_style", usize::from(self.section_view_style.is_some()));
        inventory.backing("view_rep_refs", self.view_rep_refs.len());
        inventory.backing("section_view_reps", self.section_view_reps.len());
        inventory.backing("fields", self.fields.len());
        inventory.backing("block_visibility_params", self.block_visibility_params.len());
        inventory.backing("block_representations", self.block_representations.len());
        inventory.backing("context_scales", self.context_scales.len());
        inventory.backing("dgn_ls_definitions", self.dgn_ls_definitions.len());
        inventory.backing("dgn_ls_components", self.dgn_ls_components.len());
    }
}
