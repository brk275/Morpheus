# Verify a person with SAS

This route is prepared in source. It still requires source review, the targeted
SDK/controller checks, native UI checks and real homeserver interoperability
acceptance. It does not establish beta or release readiness.

## Request a comparison

1. Open or create a direct conversation through the ordinary chat controls.
   Return to Settings → Security & recovery with that conversation selected.
2. Enter the other person's complete Matrix ID, then review the exact person
   and conversation before sending the request. Verification does not create
   a room or invite anyone. A direct-room routing flag does not prove exclusive
   or private membership.
3. Have the person accept on their device. Continue the comparison here when
   both devices are ready. Compare every displayed emoji or number using a
   separate trusted channel.
4. Check the comparison box and confirm only when all values match. Choose
   cancellation if they differ. Dismissing this view only closes it locally;
   it does not claim remote cancellation or grant trust.

Incoming requests appear in the bounded list only when this device's encryption
SDK recognizes the exact sender, recipient, room and request event. Review is
read-only; accepting and confirming each need an explicit action. Other-device
verification remains a separate control.

## Understand the result

A completed SAS can establish device trust without verifying the person's
signing identity, for example when this account's private signing keys are not
available. This UI reports signing-identity verification only when the exact
completed SAS identity receipt matches a fresh read of the same peer's full
master key and the SDK reports that identity verified. Changed identities,
device-only completion, missing receipts and pending completion stay distinct.
Recover signing keys through the existing recovery controls when needed; this
route never manually marks an identity trusted.

Requests and comparison values are bound to one selected account, view, person,
room and flow. Account switches, protected account operations, navigation and
shutdown withdraw the view. Actual owned verification work must finish before
account store removal or shutdown; cancellation flags alone are not join proof.
An in-flight error may mean a request was sent, so it is not replayed blindly.

The request book holds at most eight outgoing and 64 incoming records, displaying
at most 16 incoming rows. Open outgoing records are never evicted to admit a new
request; terminal SDK receipts or the original request lifetime reclaim space.
If incoming records were omitted, another outgoing request needs explicit
acknowledgement after checking with that person. This requirement lasts for the
last omitted request's full lifetime. No raw verification values or SDK errors
are logged.
