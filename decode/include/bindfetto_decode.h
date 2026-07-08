/*
 * bindfetto_decode.h — C ABI for the bindfetto offline decode core.
 *
 * Resolves raw Binder transaction codes in bindfetto log lines to method names
 * against a precompiled AIDL catalog. Backed by the Rust `bindfetto-decode` crate
 * (link against libbindfetto_decode.a / .so). See decode/src/ffi.rs.
 *
 * Threading: a BfDecoder is immutable after creation and bf_decode_line only borrows
 * it, so one decoder may be shared across threads. Create/free are not reentrant on
 * the same handle.
 */
#ifndef BINDFETTO_DECODE_H
#define BINDFETTO_DECODE_H

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque decoder handle. */
typedef struct BfDecoder BfDecoder;

/*
 * Build a decoder from NUL-terminated UTF-8 catalog JSON:
 *   { "android.app.IActivityManager": { "1": "getTasks", "7": "startActivity" } }
 * Returns NULL if the argument is NULL, not UTF-8, or not valid catalog JSON.
 * Free the result with bf_decoder_free.
 */
BfDecoder *bf_decoder_new(const char *catalog_json);

/* Free a decoder from bf_decoder_new. NULL is ignored. */
void bf_decoder_free(BfDecoder *decoder);

/*
 * Decode one line: rewrite each `interface.[code:N]` token whose method is known to
 * `interface.method`, leaving unknown codes and non-bindfetto lines unchanged.
 * Returns a newly-allocated NUL-terminated UTF-8 string the caller must free with
 * bf_string_free, or NULL if an argument is NULL or `line` is not UTF-8.
 */
char *bf_decode_line(const BfDecoder *decoder, const char *line);

/* Free a string from bf_decode_line. NULL is ignored. */
void bf_string_free(char *s);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* BINDFETTO_DECODE_H */
