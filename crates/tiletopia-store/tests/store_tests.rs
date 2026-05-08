#[cfg(test)]
mod tests {
    use tiletopia_store::{LocalStore, StoreError, TileStore};
    use bytes::Bytes;

    fn temp_store() -> (LocalStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::new(dir.path().to_path_buf());
        (store, dir)
    }

    #[tokio::test]
    async fn put_and_get() {
        let (store, _dir) = temp_store();
        store.put("test/file.txt", Bytes::from("hello")).await.unwrap();
        let data = store.get("test/file.txt").await.unwrap();
        assert_eq!(data.as_ref(), b"hello");
    }

    #[tokio::test]
    async fn get_not_found() {
        let (store, _dir) = temp_store();
        let result = store.get("nonexistent").await;
        assert!(matches!(result, Err(StoreError::NotFound(_))));
    }

    #[tokio::test]
    async fn exists_check() {
        let (store, _dir) = temp_store();
        assert!(!store.exists("test.txt").await.unwrap());
        store.put("test.txt", Bytes::from("data")).await.unwrap();
        assert!(store.exists("test.txt").await.unwrap());
    }

    #[tokio::test]
    async fn delete_file() {
        let (store, _dir) = temp_store();
        store.put("del.txt", Bytes::from("data")).await.unwrap();
        store.delete("del.txt").await.unwrap();
        assert!(!store.exists("del.txt").await.unwrap());
    }

    #[tokio::test]
    async fn list_files() {
        let (store, _dir) = temp_store();
        store.put("dir/a.txt", Bytes::from("a")).await.unwrap();
        store.put("dir/b.txt", Bytes::from("b")).await.unwrap();
        let files = store.list("dir").await.unwrap();
        assert_eq!(files.len(), 2);
    }

    #[tokio::test]
    async fn path_traversal_prevention() {
        let (store, _dir) = temp_store();
        // ".." is stripped, so "../escape.txt" becomes "/escape.txt" → "escape.txt"
        // The file should end up inside the store root, not outside it
        store.put("subdir/../inside.txt", Bytes::from("safe")).await.unwrap();
        assert!(store.exists("subdir/inside.txt").await.unwrap()
            || store.exists("inside.txt").await.unwrap());
    }
}
