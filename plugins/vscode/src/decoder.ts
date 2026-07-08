// JS side of the wasm decode contract. Marshals UTF-8 strings across the wasm
// boundary via the module's `bf_alloc`/`bf_free` byte allocator; see
// plugins/vscode/wasm/src/lib.rs.

interface WasmExports {
  memory: WebAssembly.Memory;
  bf_alloc(len: number): number;
  bf_free(ptr: number, len: number): void;
  bf_decoder_new(catalogPtr: number): number;
  bf_decoder_free(handle: number): void;
  bf_decode_line(handle: number, linePtr: number): number;
  bf_string_free(ptr: number): void;
}

export class BindfettoDecoder {
  private constructor(private readonly ex: WasmExports, private handle: number) {}

  /** Instantiate the wasm module and build a decoder from catalog JSON. */
  static async load(wasmBytes: Uint8Array, catalogJson: string): Promise<BindfettoDecoder> {
    // Compile then instantiate the Module (rather than the bytes overload) so the
    // result type is unambiguously an Instance across TS lib versions.
    const module = await WebAssembly.compile(wasmBytes as unknown as BufferSource);
    const instance = await WebAssembly.instantiate(module, {});
    const ex = instance.exports as unknown as WasmExports;
    const handle = withCString(ex, catalogJson, (ptr) => ex.bf_decoder_new(ptr));
    if (handle === 0) {
      throw new Error("invalid catalog JSON");
    }
    return new BindfettoDecoder(ex, handle);
  }

  /** Decode one line; returns the input unchanged if nothing resolved. */
  decodeLine(line: string): string {
    const ex = this.ex;
    return withCString(ex, line, (ptr) => {
      const outPtr = ex.bf_decode_line(this.handle, ptr);
      if (outPtr === 0) {
        return line;
      }
      const decoded = readCString(ex, outPtr);
      ex.bf_string_free(outPtr);
      return decoded;
    });
  }

  /** Release the wasm-side decoder. Safe to call more than once. */
  dispose(): void {
    if (this.handle !== 0) {
      this.ex.bf_decoder_free(this.handle);
      this.handle = 0;
    }
  }
}

/** Copy `s` (as NUL-terminated UTF-8) into wasm memory, run `fn(ptr)`, then free. */
function withCString<T>(ex: WasmExports, s: string, fn: (ptr: number) => T): T {
  const bytes = new TextEncoder().encode(s);
  const len = bytes.length + 1; // + NUL terminator
  const ptr = ex.bf_alloc(len);
  if (ptr === 0) {
    throw new Error("wasm allocation failed");
  }
  try {
    // Re-view memory after alloc: bf_alloc may have grown (and moved) the buffer.
    const mem = new Uint8Array(ex.memory.buffer);
    mem.set(bytes, ptr);
    mem[ptr + bytes.length] = 0;
    return fn(ptr);
  } finally {
    ex.bf_free(ptr, len);
  }
}

/** Read a NUL-terminated UTF-8 string from wasm memory at `ptr`. */
function readCString(ex: WasmExports, ptr: number): string {
  // Re-view: bf_decode_line may have grown the buffer while producing its output.
  const mem = new Uint8Array(ex.memory.buffer);
  let end = ptr;
  while (mem[end] !== 0) {
    end++;
  }
  return new TextDecoder().decode(mem.subarray(ptr, end));
}
