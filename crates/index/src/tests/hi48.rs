use crate::Hi48;

#[test]
fn record_ids_split_join_and_round_trip_their_bytes() {
    for id in [0, 65535, 65536, u32::MAX as u64, 1 << 32, u64::MAX] {
        let (hi48, offset) = Hi48::split(id);
        assert_eq!(hi48.record_id(offset), id);
        assert_eq!(hi48.to_bytes(), hi48.get().to_be_bytes()[2..]);
        assert_eq!(Hi48::from_bytes(hi48.to_bytes()), hi48);
        assert_eq!(Hi48::from_slice(&hi48.to_bytes()), Some(hi48));
    }
    assert_eq!(Hi48::from_slice(&[0; 5]), None);
    assert_eq!(Hi48::from_slice(&[0; 7]), None);
}

#[test]
fn only_values_up_to_48_bits_are_accepted() {
    assert_eq!(Hi48::new((1 << 48) - 1), Some(Hi48::MAX));
    assert_eq!(Hi48::new(1 << 48), None);
}
