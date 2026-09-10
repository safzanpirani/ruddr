export type RPCID = string | number | null;

export interface RPCRequest {
  id?: RPCID;
  method: string;
  params?: unknown;
}

export interface RPCError {
  code: number;
  message: string;
}

export interface RPCResponse {
  id: RPCID;
  result?: unknown;
  error?: RPCError;
}

export interface RPCNotification {
  method: string;
  params: unknown;
}

export type ProtocolMessage = RPCResponse | RPCNotification;

export interface RuddrAdapter {
  handle(request: RPCRequest): Promise<void>;
  close(): Promise<void>;
}

const MAX_LINE_BYTES = 64 * 1024 * 1024;

export async function runAppServer(
  adapter: RuddrAdapter,
  emit: (message: ProtocolMessage) => Promise<void>,
): Promise<void> {
  try {
    for await (const line of readLines(Bun.stdin.stream())) {
      if (!line.trim()) continue;
      try {
        const parsed: unknown = JSON.parse(line);
        if (!isRPCRequest(parsed)) throw new Error("JSON-RPC message must contain a method");
        await adapter.handle(parsed);
      } catch (error) {
        await emit({ id: null, error: { code: -32700, message: errorMessage(error) } });
      }
    }
  } finally {
    await adapter.close();
  }
}

export function createEmitter(): (message: ProtocolMessage) => Promise<void> {
  const stdout = Bun.stdout.writer();
  let chain = Promise.resolve();
  return (message) => {
    chain = chain.then(async () => {
      stdout.write(`${JSON.stringify(message)}\n`);
      await stdout.flush();
    });
    return chain;
  };
}

export async function* readLines(
  stream: ReadableStream<Uint8Array>,
): AsyncGenerator<string> {
  const reader = stream.getReader();
  const decoder = new TextDecoder();
  // Chunks stay unjoined until a newline arrives, and the size guard tracks a
  // running byte count. Scanning and concatenating the whole buffer on every
  // chunk made a single multi-megabyte line cost quadratic time.
  let pending: string[] = [];
  let pendingBytes = 0;
  const joinPending = (): string => {
    const buffered = pending.length === 1 ? pending[0] : pending.join("");
    pending = buffered ? [buffered] : [];
    return buffered;
  };
  try {
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      const decoded = decoder.decode(value, { stream: true });
      if (!decoded) continue;
      pending.push(decoded);
      pendingBytes += Buffer.byteLength(decoded);
      if (pendingBytes > MAX_LINE_BYTES) {
        throw new Error("JSON-RPC line exceeds 64 MiB");
      }
      if (!decoded.includes("\n")) continue;
      let buffered = joinPending();
      let newline = buffered.indexOf("\n");
      while (newline >= 0) {
        const line = buffered.slice(0, newline);
        yield line.replace(/\r$/, "");
        pendingBytes -= Buffer.byteLength(line) + 1;
        buffered = buffered.slice(newline + 1);
        newline = buffered.indexOf("\n");
      }
      pending = buffered ? [buffered] : [];
    }
    const tail = joinPending() + decoder.decode();
    if (tail) yield tail.replace(/\r$/, "");
  } finally {
    reader.releaseLock();
  }
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export function record(value: unknown, label: string): Record<string, unknown> {
  if (!isRecord(value)) throw new InvalidParamsError(`${label} must be an object`);
  return value;
}

export function requiredString(value: unknown, label: string): string {
  if (typeof value !== "string" || value.length === 0) {
    throw new InvalidParamsError(`${label} must be a non-empty string`);
  }
  return value;
}

export function optionalString(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

export function readTextInput(value: unknown): string {
  if (!Array.isArray(value)) throw new InvalidParamsError("input must be an array");
  const text = value
    .filter(isRecord)
    .filter((item) => item.type === "text" && typeof item.text === "string")
    .map((item) => item.text)
    .join("\n")
    .trim();
  if (!text) throw new InvalidParamsError("input must contain text");
  return text;
}

export function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export class InvalidParamsError extends Error {}
export class MethodNotFoundError extends Error {}

export abstract class BaseAdapter implements RuddrAdapter {
  protected initialized = false;
  protected closed = false;

  constructor(protected readonly emit: (message: ProtocolMessage) => void | Promise<void>) {}

  async handle(request: RPCRequest): Promise<void> {
    const hasID = Object.hasOwn(request, "id");
    try {
      const result = await this.dispatch(request.method, request.params);
      if (hasID) await this.emit({ id: request.id ?? null, result });
    } catch (error) {
      if (!hasID) {
        await this.emit({ method: "error", params: { error: { message: errorMessage(error) } } });
        return;
      }
      await this.emit({
        id: request.id ?? null,
        error: {
          code:
            error instanceof MethodNotFoundError
              ? -32601
              : error instanceof InvalidParamsError
                ? -32602
                : -32000,
          message: errorMessage(error),
        },
      });
    }
  }

  abstract close(): Promise<void>;
  protected abstract dispatch(method: string, params: unknown): Promise<unknown>;
}

function isRPCRequest(value: unknown): value is RPCRequest {
  return isRecord(value) && typeof value.method === "string";
}
