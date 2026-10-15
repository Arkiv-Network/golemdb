use std::error::Error;

use crate::ApiError;
use golemdb_branch::BranchError;
use golemdb_record::RecordError;

#[test]
fn api_errors_preserve_diagnostic_sources() {
    let invalid = ApiError::from(RecordError::InvalidArgument("deployment limit".into()));
    assert_eq!(invalid.to_string(), "invalid argument: deployment limit");
    assert!(invalid.source().is_none());
    let error = ApiError::from(BranchError::Storage(
        golemdb_storage::StorageError::Backend("disk error".into()),
    ));
    assert!(matches!(&error, ApiError::Internal { .. }));
    assert_eq!(error.to_string(), "internal database error");
    assert!(error.source().unwrap().to_string().contains("disk error"));
    assert!(
        error
            .source()
            .unwrap()
            .downcast_ref::<BranchError>()
            .is_some()
    );
    let error = ApiError::from(RecordError::CorruptState("missing identity"));
    assert!(
        error
            .source()
            .unwrap()
            .downcast_ref::<RecordError>()
            .is_some()
    );
    // Public errors can be shared by the same thread-safe consumers as dyn Api.
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ApiError>();
}

#[test]
fn capacity_errors_remain_public_through_internal_wrappers() {
    use golemdb_cells::CellError;
    use golemdb_index::IndexError;
    use golemdb_merkle::MerkleError;
    use golemdb_storage::StorageError;
    for error in [
        RecordError::Storage(StorageError::Full),
        RecordError::Cells(CellError::Storage(StorageError::Full)),
        RecordError::Cells(CellError::Merkle(MerkleError::Storage(StorageError::Full))),
        RecordError::Branch(BranchError::Storage(StorageError::Full)),
        RecordError::Branch(BranchError::Index(IndexError::Storage(StorageError::Full))),
        RecordError::Branch(BranchError::Index(IndexError::Merkle(
            MerkleError::Storage(StorageError::Full),
        ))),
    ] {
        assert!(matches!(ApiError::from(error), ApiError::StoreFull));
    }
}

#[test]
fn query_errors_map_to_public_errors() {
    use golemdb_branch::CommitId;
    use golemdb_cells::CellNameRef;
    use golemdb_index::TermError;
    use golemdb_query::QueryError;
    use golemdb_storage::StorageError;

    let invalid = ApiError::from(QueryError::InvalidArgument {
        field: CellNameRef::raw(b"1bad").into(),
        source: TermError::InvalidName,
    });
    assert!(matches!(&invalid, ApiError::InvalidArgument { .. }));
    assert_eq!(
        invalid.source().unwrap().downcast_ref::<TermError>(),
        Some(&TermError::InvalidName)
    );
    assert!(matches!(
        ApiError::from(QueryError::InvalidQuery { reason: "branch" }),
        ApiError::InvalidQuery { reason: "branch" }
    ));
    assert!(matches!(
        ApiError::from(QueryError::NotImplemented {
            operation: "query: sort"
        }),
        ApiError::NotImplemented {
            operation: "query: sort"
        }
    ));
    let (requested, head) = (CommitId::new(1), CommitId::new(2));
    assert!(matches!(
        ApiError::from(QueryError::CommitUnavailable { requested, head }),
        ApiError::CommitUnavailable { requested: r, head: h } if r == requested && h == head
    ));
    let internal = ApiError::from(QueryError::Internal(Box::new(RecordError::CorruptState(
        "missing identity",
    ))));
    assert!(matches!(&internal, ApiError::Internal { .. }));
    assert!(
        internal
            .source()
            .unwrap()
            .downcast_ref::<RecordError>()
            .is_some()
    );
    assert!(matches!(
        ApiError::from(QueryError::Internal(Box::new(StorageError::Full))),
        ApiError::StoreFull
    ));
}
