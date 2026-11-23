use crate::ContainerId;

#[test]
fn record_ids_split_join_and_round_trip_their_bytes() {
    for id in [0, 65535, 65536, u32::MAX as u64, 1 << 32, u64::MAX] {
        let (container_id, offset) = ContainerId::split(id);
        assert_eq!(container_id.record_id(offset), id);
        assert_eq!(
            container_id.to_bytes(),
            container_id.get().to_be_bytes()[2..]
        );
        assert_eq!(
            ContainerId::from_bytes(container_id.to_bytes()),
            container_id
        );
        assert_eq!(
            ContainerId::from_slice(&container_id.to_bytes()),
            Some(container_id)
        );
    }
    assert_eq!(ContainerId::from_slice(&[0; 5]), None);
    assert_eq!(ContainerId::from_slice(&[0; 7]), None);
}

#[test]
fn only_values_up_to_48_bits_are_accepted() {
    assert_eq!(ContainerId::new((1 << 48) - 1), Some(ContainerId::MAX));
    assert_eq!(ContainerId::new(1 << 48), None);
}
