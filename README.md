# xmip-core-archive-google-cloud-storage

Google Cloud Storage archive target: one item is one object in a bucket, its TOML metadata a second object under `.meta`, each created only where none is and never written over. A technology of [xmip-core-archive](https://github.com/IlleNilsson/xmip-core-archive).

## An archived item is never written over

An item is two objects: its bytes at `<prefix>/<data_type>/<identifier>` and its metadata, the archive capability's TOML sidecar with the SHA-256 of the bytes, at `<prefix>/.meta/<data_type>/<identifier>`. The archive is `archive::named`, the capability's one rule for a store of named objects, over the google-cloud-storage transport's client: the metadata is created first as the claim, then the bytes, then the receipt is answered, `google-cloud-storage://<bucket>/<object>` as the transport's `origin` writes it. Every object is created with `ifGenerationMatch=0`, which Cloud Storage writes only where no object of the name exists and answers `412 Precondition Failed` otherwise ([Request preconditions](https://docs.cloud.google.com/storage/docs/request-preconditions)). So an item archived again with the same bytes answers the same receipt, one with other bytes or a foreign object at its name is refused and nothing is written over, and an archive stopped between the two objects is completed by its repeat. A restore reads the metadata first: where there is none it is refused as not an item this archive completed, and the receipt's checksum, the metadata's and the bytes' must agree. A receipt naming another bucket is refused. Until 2026-11-19 both objects were uploaded over whatever lay there, the metadata at the bytes' name with `.meta` appended, so archiving `order.meta` wrote over the metadata of `order`.

## Verification

The included workflow is manual-only and calls the versioned shared workflow at
`IlleNilsson/.github@v1`.
