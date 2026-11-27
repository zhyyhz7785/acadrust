//! Isolated, read-only coverage probe. No editing or saving is performed.
//! cargo run --release --example native_data_inventory -- drawing.dwg
use acadrust::document::{NativeDataCarrier, NativeDataPart};
use acadrust::io::dwg::eed_codec::EedHandleKind;
use acadrust::xdata::XDataValue;
use acadrust::DwgReader;
use std::collections::{BTreeMap, HashSet};

#[derive(Default)]
struct AppCounts {
    structured: u64,
    structured_handles: u64,
    raw: u64,
    raw_bytes: u64,
    undecoded: u64,
    unknown_app: u64,
    binary_blocks: u64,
    layer_slots: u64,
    object_slots: u64,
    null_slots: u64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os().nth(1).ok_or("usage: native_data_inventory drawing.dwg")?;
    let doc = DwgReader::from_file(std::path::Path::new(&path))?.read()?;
    let mut raw = BTreeMap::<String, (u64, u64)>::new();
    let mut apps = BTreeMap::<(String, String), AppCounts>::new();
    let mut opaque_entities = BTreeMap::<String, u64>::new();
    let mut opaque_objects = BTreeMap::<String, u64>::new();
    let mut backing = BTreeMap::<&'static str, usize>::new();
    let mut undecoded_samples = Vec::new();
    let model: HashSet<_> = doc.entities_in_block("*Model_Space")
        .map(|e| e.common().handle).collect();
    let mut model_raw = HashSet::new();
    let mut model_xdata = HashSet::new();
    let mut model_eed = HashSet::new();
    doc.visit_native_data(|part| match part {
        NativeDataPart::RawEntity { owner, carrier, raw: record } => {
            let item = raw.entry(format!("{carrier:?}/{:?}", record.version)).or_default();
            item.0 += 1;
            item.1 += record.data.len() as u64;
            if carrier == NativeDataCarrier::Entity && model.contains(&owner) {
                model_raw.insert(owner);
            }
        }
        NativeDataPart::StructuredXdata { owner, carrier, record } => {
            let item = apps.entry((format!("{carrier:?}"), record.application_name.clone())).or_default();
            item.structured += 1;
            item.structured_handles += record.values.iter()
                .filter(|value| matches!(value, XDataValue::Handle(_))).count() as u64;
            if carrier == NativeDataCarrier::Entity && model.contains(&owner) {
                model_xdata.insert(owner);
            }
        }
        NativeDataPart::Eed(block) => {
            let app = block.application_name.map(str::to_owned)
                .unwrap_or_else(|| format!("<unresolved:{:X}>", block.application.value()));
            let item = apps.entry((format!("{:?}", block.carrier), app)).or_default();
            item.raw += 1;
            item.raw_bytes += block.bytes.len() as u64;
            if block.application_name.is_none() { item.unknown_app += 1; }
            if block.carrier == NativeDataCarrier::Entity && model.contains(&block.owner) {
                model_eed.insert(block.owner);
            }
            if let Some(inspection) = block.inspection {
                if inspection.contains_binary_data { item.binary_blocks += 1; }
                for slot in inspection.handle_slots {
                    match slot.kind {
                        EedHandleKind::Layer => item.layer_slots += 1,
                        EedHandleKind::Object => item.object_slots += 1,
                    }
                    if slot.handle.is_null() { item.null_slots += 1; }
                }
            } else {
                item.undecoded += 1;
                if undecoded_samples.len() < 24 {
                    undecoded_samples.push((block.owner, block.application, block.bytes.len(), block.encoding_version));
                }
            }
        }
        NativeDataPart::OpaqueEntity { type_name, .. } => {
            *opaque_entities.entry(type_name.to_owned()).or_default() += 1;
        }
        NativeDataPart::OpaqueObject { object, .. } => {
            // Do not dump opaque object bytes, private application data, or
            // drawing geometry. Report only type classification.
            let name = match object {
                acadrust::objects::ObjectType::Unknown { .. } => "Unknown",
                acadrust::objects::ObjectType::ProxyObject(_) => "ProxyObject",
                _ => "Other",
            };
            *opaque_objects.entry(name.to_owned()).or_default() += 1;
        }
        NativeDataPart::OtherBacking { name, count } => {
            backing.insert(name, count);
        }
    });
    println!("source_version={:?} current_version={:?}", doc.dwg_source_version, doc.version);
    println!("flat_entities={} objects={} model_members={}", doc.entity_count(), doc.objects.len(), model.len());
    println!("model_raw={} model_structured_xdata={} model_raw_eed={}",
        model_raw.len(), model_xdata.len(), model_eed.len());
    for (carrier, (count, bytes)) in raw {
        println!("raw carrier={carrier} records={count} bytes={bytes}");
    }
    for ((carrier, app), v) in apps {
        println!("app carrier={carrier} name={app:?} structured={} structured_handles={} raw_blocks={} raw_bytes={} undecoded={} unresolved_app={} binary_blocks={} layer_slots={} object_slots={} null_slots={}",
            v.structured, v.structured_handles, v.raw, v.raw_bytes, v.undecoded,
            v.unknown_app, v.binary_blocks, v.layer_slots, v.object_slots, v.null_slots);
    }
    println!("opaque_entities={opaque_entities:?}");
    println!("opaque_objects={opaque_objects:?}");
    println!("unqualified_backing={backing:?}");
    println!("undecoded_samples={undecoded_samples:?}");
    println!("coverage=PARTIAL; decoded slot counts are not a complete reference audit or safe-edit certificate");
    Ok(())
}
