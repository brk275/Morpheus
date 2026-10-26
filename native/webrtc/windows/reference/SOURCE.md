# Original Windows source-selection DATA

`root-DEPS`, `LICENSE.webrtc`, and `PATENTS.webrtc` are unmodified files from
webrtc-sdk/webrtc commit `aaeeee8077eb0a4cad1c9494e9c6433c751ef663`.
`buildtools-DEPS` and `LICENSE.buildtools` are unmodified files from Chromium
buildtools commit `aec4f3c79e5af648386819cbe7f6e67206385ece`.
Their exact source Git blob identities are bound by the Windows source-plan
selection and private original-source receipt. These copies support the
existing restricted DEPS parser regression; they are never executed as Python.

The Windows selection independently evaluates the original Windows/x64
conditions. It retains 56 exact Git repositories, 22 uninitialized gitlinks,
and 69 explicitly declared alias normalizations. Root eight security patches
precede the original SDK Windows five patches. No generated metadata, tools,
hooks or third-party submodule source is fabricated by these declarations.

The source profile authorizes no execution. Actual exported Git/patch receipts,
non-Git tool/runtime closure, MSVC/Windows SDK host admission, generated inputs,
native tests, wrapper ABI, complete license/preferred-source and distribution
acceptance remain separate required gates. The selected native baseline is
prospective; it is not attributed as the source of any old prebuilt archive.
