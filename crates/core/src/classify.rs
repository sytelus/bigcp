//! Pure implementation of the skip and replacement decision table.

use bigcp_win::ObjectKind;

use crate::model::{Classification, EntrySnapshot};

/// Classifies a source entry against its case-insensitive destination twin.
#[must_use]
pub fn classify(
    source: &EntrySnapshot,
    destination: Option<&EntrySnapshot>,
    replace: bool,
) -> Classification {
    let Some(destination) = destination else {
        return Classification::New;
    };
    if type_conflict(source.kind(), destination.kind())
        || (source.kind() == ObjectKind::Reparse
            && source.metadata.reparse_tag != destination.metadata.reparse_tag)
    {
        return Classification::TypeConflict;
    }

    let mut data_differences = Vec::new();
    if source.metadata.size != destination.metadata.size {
        data_differences.push("size");
    }
    if source.metadata.basic.last_write_time != destination.metadata.basic.last_write_time {
        data_differences.push("mtime");
    }
    if !data_differences.is_empty() {
        let destination_newer =
            destination.metadata.basic.last_write_time > source.metadata.basic.last_write_time;
        return if replace {
            Classification::Replace {
                fields: data_differences,
                destination_newer,
            }
        } else {
            Classification::SkipDifferent {
                fields: data_differences,
                destination_newer,
            }
        };
    }

    let mut metadata_differences = Vec::new();
    if source.copyable_attributes() != destination.copyable_attributes() {
        metadata_differences.push("attributes");
    }
    if source.metadata.basic.creation_time != destination.metadata.basic.creation_time {
        metadata_differences.push("ctime");
    }
    if source.metadata.ea_size != destination.metadata.ea_size {
        metadata_differences.push("ea_size");
    }
    if metadata_differences.is_empty() {
        Classification::Same
    } else {
        Classification::MetadataDiff(metadata_differences)
    }
}

fn type_conflict(source: ObjectKind, destination: ObjectKind) -> bool {
    source != destination
}

#[cfg(test)]
mod tests {
    use super::classify;
    use crate::model::{Classification, EntrySnapshot};
    use bigcp_win::{BasicMetadata, FileIdentity, ObjectKind, ObjectMetadata};
    use std::path::PathBuf;

    fn entry(size: u64, mtime: i64, attributes: u32, kind: ObjectKind) -> EntrySnapshot {
        EntrySnapshot {
            relative_path: PathBuf::from("file"),
            metadata: ObjectMetadata {
                identity: FileIdentity {
                    volume_serial: 1,
                    file_id: [0; 16],
                },
                kind,
                size,
                allocation_size: size,
                ea_size: 0,
                basic: BasicMetadata {
                    creation_time: 10,
                    last_access_time: 11,
                    last_write_time: mtime,
                    attributes,
                },
                reparse_tag: None,
            },
        }
    }

    #[test]
    fn exact_filetime_tick_is_different() {
        let source = entry(1, 100, 0, ObjectKind::File);
        let destination = entry(1, 101, 0, ObjectKind::File);
        assert!(matches!(
            classify(&source, Some(&destination), true),
            Classification::Replace { .. }
        ));
    }

    #[test]
    fn attribute_only_difference_is_metadata_repair() {
        let source = entry(1, 100, 1, ObjectKind::File);
        let destination = entry(1, 100, 0, ObjectKind::File);
        assert!(matches!(
            classify(&source, Some(&destination), true),
            Classification::MetadataDiff(_)
        ));
    }

    #[test]
    fn replacement_can_be_withheld() {
        let source = entry(2, 100, 0, ObjectKind::File);
        let destination = entry(1, 100, 0, ObjectKind::File);
        assert!(matches!(
            classify(&source, Some(&destination), false),
            Classification::SkipDifferent { .. }
        ));
    }

    #[test]
    fn ea_size_only_difference_is_metadata_repair() {
        let source = entry(1, 100, 0, ObjectKind::File);
        let mut destination = entry(1, 100, 0, ObjectKind::File);
        destination.metadata.ea_size = 16;
        assert!(matches!(
            classify(&source, Some(&destination), true),
            Classification::MetadataDiff(fields) if fields == ["ea_size"]
        ));
    }
}
