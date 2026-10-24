// Morpheus same-version owned notification ABI. No Rust context or native pointer
// crosses this boundary. All borrowed input bytes are consumed before return.
#include <stddef.h>
#include <stdint.h>

typedef struct {
    const uint8_t* bytes;
    uint64_t length;
} MorpheusOwnedBytes;

typedef struct {
    uint32_t slot;
    uint32_t status;
    uint64_t cookie;
    uint32_t stage;
    uint32_t reserved;
} MorpheusOwnedResult;

_Static_assert(sizeof(void*) == 8, "owned notifications require supported 64-bit macOS");
_Static_assert(sizeof(MorpheusOwnedBytes) == 16, "owned bytes ABI size");
_Static_assert(offsetof(MorpheusOwnedBytes, length) == 8, "owned bytes ABI offset");
_Static_assert(sizeof(MorpheusOwnedResult) == 24, "owned result ABI size");
_Static_assert(_Alignof(MorpheusOwnedResult) == 8, "owned result ABI alignment");
_Static_assert(offsetof(MorpheusOwnedResult, cookie) == 8, "owned cookie ABI offset");
_Static_assert(offsetof(MorpheusOwnedResult, stage) == 16, "owned stage ABI offset");

void morpheus_owned_prepare(const uint8_t* identity, MorpheusOwnedBytes title,
                           MorpheusOwnedBytes subtitle, MorpheusOwnedBytes body,
                           uint32_t sound, MorpheusOwnedResult* output);
void morpheus_owned_submit(uint32_t slot, uint64_t cookie, const uint8_t* identity,
                          MorpheusOwnedResult* output);
void morpheus_owned_retire(uint32_t slot, uint64_t cookie, const uint8_t* identity,
                          uint32_t withdraw, MorpheusOwnedResult* output);
void morpheus_owned_abandon(uint32_t slot, uint64_t cookie, const uint8_t* identity);

// Called synchronously with an exact 16-byte stack identity. Kinds are fixed:
// 0=contents click, 1=dismissal/unsupported activation, 2=decode failure.
extern void morpheus_owned_response(const uint8_t* identity, uint32_t kind);
