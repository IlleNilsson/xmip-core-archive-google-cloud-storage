#![forbid(unsafe_code)]

//! Google Cloud Storage archive: an [`ArchiveStore`] that keeps each
//! retained item as one object in a bucket, its metadata as a second object
//! beside it, and restores the item by getting both back.
//!
//! What an item is in a store of named objects is the archive capability's
//! one rule, [`named`] (ADR-0044): the bytes at
//! `<prefix>/<data_type>/<identifier>`, the metadata — the TOML sidecar
//! with the SHA-256 of the bytes — at `<prefix>/.meta/<data_type>/<identifier>`,
//! the metadata created first as the claim, then the bytes, then the
//! receipt. This crate is the [`Named`] store it runs on: a create is an
//! upload with `ifGenerationMatch=0`, which Cloud Storage writes only where
//! no object of the name exists, and a read is a get of whichever
//! generation lies there.
//!
//! A xmip-core-archive **technology** (repository-model.md): it depends on
//! the archive capability for the [`ArchiveStore`] trait and its item,
//! receipt and error types, and on the google-cloud-storage transport
//! technology for the requests — the JSON API with a bearer token.
//! Obtaining the token is outside this crate, as it is outside the
//! transport's: the archive is configured with one.
//!
//! An archive never deletes and never writes over (ADR-0040): this one
//! creates and gets, nothing else. The receipt is
//! `google-cloud-storage://<bucket>/<object>`, the transport's origin for
//! the object, written by its `origin`, with the SHA-256 of the bytes as
//! its checksum, and restoring checks the bytes that come back against it
//! and the metadata.

use std::time::Duration;

use archive::named::{self, Created, Named};
use archive::{ArchiveError, ArchiveItem, ArchiveReceipt, ArchiveStore, location};
use google_cloud_storage::Client;

/// An archive that keeps items as objects under one prefix of one bucket.
pub struct GcsArchive {
    endpoint: String,
    token: String,
    bucket: String,
    prefix: String,
    timeout: Option<Duration>,
}

impl GcsArchive {
    /// An archive writing to `bucket` at `endpoint` —
    /// `https://storage.googleapis.com` in the cloud, `http://host:port` for
    /// an emulator — presenting `token` as the bearer, under no prefix.
    #[must_use]
    pub fn new(
        endpoint: impl Into<String>,
        token: impl Into<String>,
        bucket: impl Into<String>,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            token: token.into(),
            bucket: bucket.into(),
            prefix: String::new(),
            timeout: None,
        }
    }

    /// The prefix every object name is laid out under, `retained` say.
    #[must_use]
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// Give up on an endpoint that stops answering.
    #[must_use]
    pub const fn timing_out_after(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// The bucket as one client speaks to it, for one archive or restore:
    /// its requests share that client's connections.
    fn bucket(&self) -> Result<Bucket<'_>, ArchiveError> {
        let client = Client::new(&self.endpoint, &self.token).map_err(ArchiveError::caused_by)?;
        let client = match self.timeout {
            Some(timeout) => client.timing_out_after(timeout),
            None => client,
        };
        Ok(Bucket {
            client,
            name: &self.bucket,
        })
    }
}

impl ArchiveStore for GcsArchive {
    fn archive(&self, item: ArchiveItem) -> Result<ArchiveReceipt, ArchiveError> {
        named::archive(&self.bucket()?, &self.prefix, item)
    }

    fn restore(&self, receipt: &ArchiveReceipt) -> Result<ArchiveItem, ArchiveError> {
        named::restore(&self.bucket()?, &self.prefix, receipt)
    }
}

/// The archive's bucket, through one client.
struct Bucket<'a> {
    client: Client,
    name: &'a str,
}

impl Named for Bucket<'_> {
    /// An upload with `ifGenerationMatch=0`: Cloud Storage answers once the
    /// object is stored, `412` where one of the name exists.
    fn create(&self, key: &str, bytes: &[u8]) -> Result<Created, ArchiveError> {
        let written = self
            .client
            .put_new(self.name, key, bytes)
            .map_err(ArchiveError::caused_by)?;
        Ok(if written.is_some() {
            Created::Created
        } else {
            Created::Exists
        })
    }

    fn read(&self, key: &str) -> Result<Option<Vec<u8>>, ArchiveError> {
        self.client
            .read(self.name, key)
            .map_err(ArchiveError::caused_by)
    }

    fn location(&self, key: &str) -> String {
        google_cloud_storage::origin(self.name, key)
    }

    /// The object a receipt names, in this archive's bucket only: every
    /// read is made there.
    fn key(&self, location: &str) -> Result<String, ArchiveError> {
        let (bucket, key) = location::bucket_key("google-cloud-storage", location)?;
        if bucket != self.name {
            return Err(ArchiveError {
                message: format!(
                    "{location} is not in {}, the bucket this archive keeps",
                    self.name
                ),
            });
        }
        Ok(key.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use archive::checksum;
    use archive::fixture::{item, secs};
    use archive::sidecar::Sidecar;
    use google_cloud_storage::{Event, Session};
    use std::assert_matches;
    use std::net::TcpListener;
    use std::thread::JoinHandle;

    /// A far end that answers `requests` bearing the token, one connection
    /// each, and then hands back what it holds and what it saw.
    fn far_end(requests: usize) -> (String, JoinHandle<(Session, Vec<Event>)>) {
        far_end_holding(&[], requests)
    }

    /// [`far_end`], holding `objects` in the archive's bucket first.
    fn far_end_holding(
        objects: &[(&str, &[u8])],
        requests: usize,
    ) -> (String, JoinHandle<(Session, Vec<Event>)>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address").to_string();
        let held = objects
            .iter()
            .map(|(name, bytes)| (format!("orders-archive/{name}"), bytes.to_vec()))
            .collect();
        let handle = std::thread::spawn(move || {
            let mut session = Session::new("ya29.token")
                .with_objects(held)
                .timing_out_after(secs(2));
            let events = (0..requests)
                .map(|_| session.serve_one(&listener).expect("served"))
                .collect();
            (session, events)
        });
        (address, handle)
    }

    fn store(address: &str) -> GcsArchive {
        GcsArchive::new(format!("http://{address}"), "ya29.token", "orders-archive")
            .with_prefix("retained")
            .timing_out_after(secs(2))
    }

    #[test]
    fn an_archived_item_is_its_metadata_then_its_bytes_and_the_receipt_names_the_bytes() {
        let (address, far_end) = far_end(2);
        let original = item("json#1");
        let receipt = store(&address).archive(original.clone()).expect("archive");
        assert_eq!(
            receipt.location,
            "google-cloud-storage://orders-archive/retained/json/json#1"
        );
        let checksum = checksum::sha256_hex(&original.bytes);
        assert_eq!(receipt.checksum.as_deref(), Some(checksum.as_str()));
        let (session, events) = far_end.join().expect("thread");
        let held = session.objects();
        assert_eq!(held.len(), 2);
        assert_eq!(
            held.get("orders-archive/retained/json/json#1"),
            Some(&original.bytes)
        );
        let meta = held
            .get("orders-archive/retained/.meta/json/json#1")
            .expect("the metadata beside it");
        let sidecar = Sidecar::parse(std::str::from_utf8(meta).expect("text")).expect("TOML");
        assert_eq!(
            (sidecar.identifier, sidecar.checksum, sidecar.metadata),
            ("json#1".to_string(), checksum, original.metadata)
        );
        assert_matches!(
            &events[0],
            Event::Stored(claim) if claim.origin_uri.ends_with("/retained/.meta/json/json#1"),
            "the claim first"
        );
        assert_matches!(
            &events[1],
            Event::Stored(arrived) if arrived.origin_uri == receipt.location,
            "the receipt names the object as the transport's origin does"
        );
    }

    #[test]
    fn the_objects_restore_the_item_through_the_same_session() {
        let (address, far_end) = far_end(4);
        let store = store(&address);
        let original = item("json#2");
        let receipt = store.archive(original.clone()).expect("archive");
        let restored = store.restore(&receipt).expect("restore");
        assert_eq!(restored, original, "both objects read back give the item");
        let (_, events) = far_end.join().expect("thread");
        let at = |name: &str| {
            Event::Retrieved(format!(
                "google-cloud-storage://orders-archive/retained/{name}"
            ))
        };
        assert_eq!(events[2..], [at(".meta/json/json#2"), at("json/json#2")]);
    }

    #[test]
    fn a_missing_object_a_changed_checksum_a_wrong_token_and_a_foreign_receipt_are_refused() {
        // Two creates; two reads; a read; a create refused.
        let (address, far_end) = far_end(6);
        let store = store(&address);
        let mut receipt = store.archive(item("json#3")).expect("archive");
        receipt.checksum = Some("0".repeat(64));
        let changed = store.restore(&receipt).expect_err("checksum");
        assert!(changed.message.contains("checksum mismatch"), "{changed}");
        receipt.location = "google-cloud-storage://orders-archive/retained/json/none".to_string();
        receipt.checksum = None;
        let missing = store.restore(&receipt).expect_err("missing");
        assert!(
            missing
                .message
                .contains("is not an item this archive completed"),
            "{missing}"
        );
        let other = GcsArchive::new(format!("http://{address}"), "other", "orders-archive")
            .timing_out_after(secs(2));
        let refused = other.archive(item("json#4")).expect_err("wrong token");
        assert!(refused.message.contains("401"), "{refused}");
        far_end.join().expect("thread");
        for location in [
            "aws-s3://bucket/key",
            "google-cloud-storage://bucket",
            "google-cloud-storage://orders-archive/elsewhere/n",
            "google-cloud-storage://orders-archive/retained/.meta/json/json#3",
        ] {
            receipt.location = location.to_string();
            assert!(store.restore(&receipt).is_err(), "{location}");
        }
        receipt.location = "google-cloud-storage://another/retained/json/json#3".to_string();
        let elsewhere = store.restore(&receipt).expect_err("another bucket");
        assert!(
            elsewhere
                .message
                .contains("is not in orders-archive, the bucket this archive keeps"),
            "{elsewhere}"
        );
    }

    #[test]
    fn an_interrupted_pair_is_completed_by_its_repeat_and_refuses_another_item() {
        let mut original = item("json#7");
        original.data_type = "text/plain".to_string();
        let claim = Sidecar {
            data_type: original.data_type.clone(),
            identifier: original.identifier.clone(),
            archived_at: archive::timestamp::now(),
            checksum: checksum::sha256_hex(&original.bytes),
            metadata: original.metadata.clone(),
        };
        // What an archive stopped between its two objects leaves: the
        // metadata, no bytes. A create found there, its read, a create; two
        // reads; twice a create found there and its read.
        let (address, far_end) = far_end_holding(
            &[(
                "retained/.meta/text_plain/json#7",
                claim.to_toml().as_bytes(),
            )],
            9,
        );
        let store = store(&address);
        let receipt = store.archive(original.clone()).expect("completed");
        assert_eq!(store.restore(&receipt).expect("restored"), original);
        let mut other_bytes = original.clone();
        other_bytes.bytes = b"another".to_vec();
        let refused = store.archive(other_bytes).expect_err("the name is taken");
        assert!(
            refused
                .message
                .contains("is archived already with other bytes"),
            "{refused}"
        );
        let mut other_type = original.clone();
        other_type.data_type = "text_plain".to_string();
        let refused = store.archive(other_type).expect_err("laid out alike");
        assert!(
            refused
                .message
                .contains("is laid out at the same name and is refused"),
            "{refused}"
        );
        let (session, _) = far_end.join().expect("thread");
        assert_eq!(session.objects().len(), 2, "the pair, and nothing else");
    }

    #[test]
    fn an_identifier_archived_again_with_the_same_bytes_answers_the_same_receipt() {
        // Two creates; two creates found there, each read; two reads.
        let (address, far_end) = far_end(8);
        let store = store(&address);
        let original = item("json#5");
        let first = store.archive(original.clone()).expect("archived");
        let again = store.archive(original.clone()).expect("archived again");
        assert_eq!(again, first);
        assert_eq!(store.restore(&again).expect("restored"), original);
        let (session, events) = far_end.join().expect("thread");
        assert_eq!(session.objects().len(), 2);
        assert_eq!(
            events[2],
            Event::Refused("conditionNotMet".to_string()),
            "nothing is written over"
        );
    }

    #[test]
    fn an_identifier_archived_again_with_other_bytes_is_refused_and_the_earlier_receipt_restores() {
        // Two creates; a create found there and its read; two reads.
        let (address, far_end) = far_end(6);
        let store = store(&address);
        let original = item("json#6");
        let receipt = store.archive(original.clone()).expect("archived");
        let mut other = original.clone();
        other.bytes = b"{\"kept\":false}".to_vec();
        let refused = store.archive(other).expect_err("never written over");
        assert!(
            refused
                .message
                .contains("is archived already with other bytes"),
            "{refused}"
        );
        assert_eq!(store.restore(&receipt).expect("restored"), original);
        far_end.join().expect("thread");
    }

    #[test]
    fn order_and_order_meta_are_two_items() {
        // Two creates each; two reads each.
        let (address, far_end) = far_end(8);
        let store = store(&address);
        let order = item("order");
        let mut order_meta = item("order.meta");
        order_meta.bytes = b"{\"meta\":true}".to_vec();
        order_meta.metadata = vec![("source".to_string(), "elsewhere".to_string())];
        let first = store.archive(order.clone()).expect("order");
        let second = store.archive(order_meta.clone()).expect("order.meta");
        assert_eq!(store.restore(&first).expect("order"), order);
        assert_eq!(store.restore(&second).expect("order.meta"), order_meta);
        let (session, _) = far_end.join().expect("thread");
        assert_eq!(session.objects().len(), 4, "two objects each");
    }

    #[test]
    fn bytes_without_metadata_are_refused_on_restore() {
        let bytes = b"left by hand".as_slice();
        // The metadata's read.
        let (address, far_end) = far_end_holding(&[("retained/json/json#8", bytes)], 1);
        let receipt = ArchiveReceipt {
            location: "google-cloud-storage://orders-archive/retained/json/json#8".to_string(),
            checksum: Some(checksum::sha256_hex(bytes)),
        };
        let refused = store(&address).restore(&receipt).expect_err("no metadata");
        assert!(
            refused
                .message
                .contains("is not an item this archive completed"),
            "{refused}"
        );
        far_end.join().expect("thread");
    }

    #[test]
    fn a_foreign_object_at_the_key_is_refused() {
        let foreign = b"not the archive's".as_slice();
        // A create, a create found there and its read; a create found there
        // and its read.
        let (address, far_end) = far_end_holding(
            &[
                ("retained/json/json#9", foreign),
                ("retained/.meta/json/json#10", foreign),
            ],
            5,
        );
        let store = store(&address);
        let refused = store.archive(item("json#9")).expect_err("bytes there");
        assert!(
            refused
                .message
                .contains("holds bytes this archive did not write there"),
            "{refused}"
        );
        let refused = store.archive(item("json#10")).expect_err("metadata there");
        assert!(
            refused.message.contains("is not an archive's metadata"),
            "{refused}"
        );
        let (session, _) = far_end.join().expect("thread");
        for name in ["retained/json/json#9", "retained/.meta/json/json#10"] {
            assert_eq!(
                session.objects().get(&format!("orders-archive/{name}")),
                Some(&foreign.to_vec()),
                "{name} is left as it was"
            );
        }
    }

    #[test]
    fn a_data_type_beginning_with_a_dot_is_refused() {
        let mut dotted = item("json#11");
        dotted.data_type = ".meta".to_string();
        // Nothing listens there: the refusal is made before any request.
        let refused = store("127.0.0.1:1").archive(dotted).expect_err("dotted");
        assert!(refused.message.contains("begins with a dot"), "{refused}");
    }
}
