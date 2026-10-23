# Native update identity

Native packages use a closed, immutable protocol 2 identity containing product,
version, architecture and persistence cohort. Mac and Windows store it in a
read-only executable section; Linux binds the same fields to its AppImage footer.
Mac and Windows package producers parse these records as data without launching
the executable. The Linux producer invokes the reviewed, just-built binary
with its early metadata-only command, before UI, account storage or the
credential vault is opened. It appends the validated metadata to the AppImage
before the complete artifact is signed; preparing the footer does not sign it.

Protocol 2 separates helpers with the reviewed ordinary-shutdown handoff guards
from legacy helpers. A running installation uses its installed helper to apply
an update, so a newly fixed candidate alone cannot repair an older helper.
Legacy protocol 1 helpers reject protocol 2 candidates before replacement. New
protocol 2 helpers also reject protocol 1 candidates. There is no permissive
fallback or runtime setting to enable a cross-protocol update.

An installation with a legacy identity requires a separately verified initial or
manual installation. An unresolved existing transaction must retain its recovery
material and fail closed; this identity version change does not establish safe
automatic recovery after an unrelated manual replacement. Preserve the existing
installation and backups until its specific recovery path is verified.

The executable identity version is separate from journal schemas, Windows payload
and marker formats, acceptance plans and the persistence-cohort hash algorithm.
Those formats keep their own versions and validation rules. The identity change
does not authorize a persistence migration or a rollback through incompatible
account-store code.

Identity admission is one update precondition. It does not prove native device
cleanup, installation, interrupted recovery, signing or notarization. Installation
admission remains closed where its platform release gate is closed, and release
feeds and credentials remain absent until the required checks pass.
