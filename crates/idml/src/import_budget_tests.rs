use std::io::{Cursor, Write};
use zip::write::SimpleFileOptions;

use super::*;

fn package(parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in parts {
        zip.start_file(*name, SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated)).unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

#[test]
fn actual_read_limit_applies_even_when_the_header_claims_zero() {
    let mut reader = Cursor::new(vec![0; 33]);
    let error = read_part(&mut reader, "unused", 0, 32).unwrap_err();
    assert!(error.to_string().contains("32-byte import budget"));
    assert_eq!(reader.position(), 33);
}

#[test]
fn unused_compressed_parts_have_an_uncompressed_budget() {
    let bytes = package(&[("unused", &[0; 4096])]);
    assert!(bytes.len() < 4096);
    let limits = ImportLimits { max_part_bytes: 1024, ..ImportLimits::default() };
    assert!(read_package(&bytes, limits).unwrap_err().to_string().contains("import budget"));
}

#[test]
fn large_declared_sizes_do_not_reserve_empty_part_buffers() {
    let mut reader = Cursor::new(Vec::<u8>::new());
    let bytes = read_part(&mut reader, "empty", 16 << 20, 32 << 20).unwrap();
    assert!(bytes.is_empty());
    assert!(bytes.capacity() <= 4096);
}

#[test]
fn total_budget_counts_all_parts_and_accepts_its_exact_boundary() {
    let bytes = package(&[("first", &[1; 16]), ("second", &[2; 16])]);
    let limits = ImportLimits { max_part_bytes: 16, max_total_bytes: 32, ..ImportLimits::default() };
    assert_eq!(read_package(&bytes, limits).unwrap().len(), 2);
    assert!(read_package(&bytes, ImportLimits { max_total_bytes: 31, ..limits }).is_err());
}

#[test]
fn archive_size_and_entry_count_are_bounded() {
    let bytes = package(&[("first", b"1"), ("second", b"2")]);
    assert!(read_package(&bytes, ImportLimits { max_entries: 1, ..ImportLimits::default() }).is_err());
    assert!(read_package(&bytes, ImportLimits { max_archive_bytes: bytes.len() - 1, ..ImportLimits::default() }).is_err());
    assert!(read_package(&package(&[]), ImportLimits { max_entries: 0, ..ImportLimits::default() }).unwrap().is_empty());
}

fn footer(bytes: &[u8]) -> usize {
    bytes.windows(4).rposition(|magic| magic == b"PK\x05\x06").unwrap()
}

fn zip64(bytes: &[u8], entries: u64) -> Vec<u8> {
    let footer = footer(bytes);
    let size = u32::from_le_bytes(bytes[footer + 12..footer + 16].try_into().unwrap());
    let offset = u32::from_le_bytes(bytes[footer + 16..footer + 20].try_into().unwrap());
    let mut output = bytes[..footer].to_vec();
    let mut record = vec![0; 56];
    record[..4].copy_from_slice(b"PK\x06\x06");
    record[4..12].copy_from_slice(&44u64.to_le_bytes());
    record[12..14].copy_from_slice(&45u16.to_le_bytes());
    record[14..16].copy_from_slice(&45u16.to_le_bytes());
    record[24..32].copy_from_slice(&entries.to_le_bytes());
    record[32..40].copy_from_slice(&entries.to_le_bytes());
    record[40..48].copy_from_slice(&u64::from(size).to_le_bytes());
    record[48..56].copy_from_slice(&u64::from(offset).to_le_bytes());
    output.extend(record);
    let mut locator = vec![0; 20];
    locator[..4].copy_from_slice(b"PK\x06\x07");
    locator[8..16].copy_from_slice(&(footer as u64).to_le_bytes());
    locator[16..20].copy_from_slice(&1u32.to_le_bytes());
    output.extend(locator);
    let mut record = bytes[footer..].to_vec();
    record[8..12].fill(0xff);
    record[12..20].fill(0xff);
    output.extend(record);
    output
}

#[test]
fn forged_footer_counts_are_rejected_before_zip_index_allocation() {
    let mut bytes = package(&[("first", b"1")]);
    let footer = footer(&bytes);
    bytes[footer + 8..footer + 12].copy_from_slice(&[0x10, 0x27, 0x10, 0x27]); // 10,000 claimed entries, only one real entry.
    let error = read_package(&bytes, ImportLimits { max_entries: 3, ..ImportLimits::default() }).unwrap_err();
    assert!(error.to_string().contains("3-entry import budget"));
}

#[test]
fn zip64_counts_are_bounded_and_valid_small_zip64_packages_still_read() {
    let bytes = package(&[("first", b"1")]);
    let limits = ImportLimits { max_entries: 3, ..ImportLimits::default() };
    assert_eq!(read_package(&zip64(&bytes, 1), limits).unwrap().get("first").unwrap(), b"1");
    let mut without_sentinels = zip64(&bytes, 1);
    let classic = footer(&without_sentinels);
    let original = footer(&bytes);
    without_sentinels[classic + 8..classic + 20].copy_from_slice(&bytes[original + 8..original + 20]);
    assert_eq!(read_package(&without_sentinels, limits).unwrap().get("first").unwrap(), b"1");
    let error = read_package(&zip64(&bytes, u64::MAX), limits).unwrap_err();
    assert!(error.to_string().contains("3-entry import budget"));
}

#[test]
fn classic_counts_cannot_bypass_the_budget_when_a_zip64_locator_is_present() {
    let plain = package(&[("first", b"1")]);
    let plain_footer = footer(&plain);
    let mut bytes = zip64(&plain, 1);
    let footer = footer(&bytes);
    bytes[footer + 8..footer + 12].copy_from_slice(&[0x10, 0x27, 0x10, 0x27]);
    bytes[footer + 12..footer + 20].copy_from_slice(&plain[plain_footer + 12..plain_footer + 20]);
    let error = read_package(&bytes, ImportLimits { max_entries: 3, ..ImportLimits::default() }).unwrap_err();
    assert!(error.to_string().contains("3-entry import budget"));
}

#[test]
fn a_short_zip64_record_cannot_retry_an_older_unvalidated_footer() {
    let mut bytes = package(&[("old", b"old")]);
    let prefix = bytes.len();
    let plain = package(&[("new", b"new")]);
    let record = footer(&plain);
    let start = u32::from_le_bytes(plain[record + 16..record + 20].try_into().unwrap()) as usize;
    let mut newer = zip64(&plain, 1);
    newer[record + 48..record + 56].copy_from_slice(&((prefix + start) as u64).to_le_bytes());
    newer[record + 64..record + 72].copy_from_slice(&((prefix + record) as u64).to_le_bytes());
    newer.insert(record + 56, 0); // Length no longer covers the gap before its locator.
    bytes.extend(newer);
    let config = zip::read::Config { archive_offset: zip::read::ArchiveOffset::Known(0) };
    let ordinary = zip::ZipArchive::with_config(config, Cursor::new(&bytes)).unwrap();
    assert_eq!(ordinary.name_for_index(0), Some("old"), "fixture must exercise ZIP64 metadata fallback");
    assert!(read_package(&bytes, ImportLimits::default()).is_err());
}

#[test]
fn oversized_zip64_metadata_length_is_rejected_without_reserving_it() {
    let bytes = package(&[("first", b"1")]);
    let record = footer(&bytes);
    let mut bytes = zip64(&bytes, 1);
    bytes[record + 4..record + 12].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(read_package(&bytes, ImportLimits::default()).is_err());
}

#[test]
fn a_footer_signature_in_the_archive_comment_cannot_override_the_count() {
    let mut bytes = package(&[("first", b"1")]);
    let footer = footer(&bytes);
    bytes[footer + 20..footer + 22].copy_from_slice(&22u16.to_le_bytes());
    let mut comment = vec![0; 22];
    comment[..4].copy_from_slice(b"PK\x05\x06");
    comment[8..12].fill(0xff);
    comment[20..22].copy_from_slice(&1u16.to_le_bytes()); // Not a footer ending at EOF.
    bytes.extend(comment);
    assert_eq!(read_package(&bytes, ImportLimits::default()).unwrap().get("first").unwrap(), b"1");
}

#[test]
fn malformed_directory_cannot_retry_an_older_unvalidated_footer() {
    let mut bytes = package(&[("old", b"old")]);
    let prefix = bytes.len();
    let mut newer = package(&[("new", b"new")]);
    let footer = footer(&newer);
    let start = u32::from_le_bytes(newer[footer + 16..footer + 20].try_into().unwrap()) as usize;
    newer[footer + 16..footer + 20].copy_from_slice(&((prefix + start) as u32).to_le_bytes());
    newer[start..start + 4].fill(0); // Corrupt only the newer directory.
    bytes.extend(newer);
    let config = zip::read::Config { archive_offset: zip::read::ArchiveOffset::Known(0) };
    let ordinary = zip::ZipArchive::with_config(config, Cursor::new(&bytes)).unwrap();
    assert_eq!(ordinary.name_for_index(0), Some("old"), "fixture must exercise metadata fallback");
    assert!(read_package(&bytes, ImportLimits::default()).is_err());
}
