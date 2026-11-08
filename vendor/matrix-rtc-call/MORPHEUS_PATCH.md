# Typed MatrixRTC publication carrier

Upstream: element-hq/matrix-rust-rtc at 53880ef1ef66b62df555ce71ccd1d6c85b813596, crate matrix-rtc-call 0.5.0-rc.2. See UPSTREAM.json for each authenticated upstream blob and checksum. Both upstream licenses and original crate/workspace manifests are included unchanged.

The manifest expands upstream workspace inheritance to the same package, dependency versions and features. No dependencies, build hooks, installers or generated binaries are introduced.

JoinTransport::PublishTransport carries a typed standard LiveKit service or homeserver-mediated CS API locator. Publish(LiveKitTransport) remains available. Advertised selection uses the core's standard raw descriptor parser, preserves preference order among supported transports and rejects unknown explicit types. Authorization, endpoint policy and credentials remain owned by the media/auth adapter. No wire marker or implicit protocol fallback is added.

The production actor, room state, reactions, notification, delayed-leave and cancellation source files remain byte-identical to the locked upstream revision. Existing actor tests gain a regression for standard locator serialization, duplicate/wrong-application admission and delayed-leave cancellation. No test has been run for this private proposal.
