import { expect, test } from "bun:test";
import { readLines } from "./protocol";

function streamOf(chunks: Array<string | Uint8Array>): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder();
  let index = 0;
  return new ReadableStream({
    pull(controller) {
      if (index >= chunks.length) {
        controller.close();
        return;
      }
      const chunk = chunks[index++];
      controller.enqueue(typeof chunk === "string" ? encoder.encode(chunk) : chunk);
    },
  });
}

async function collect(chunks: Array<string | Uint8Array>): Promise<string[]> {
  const lines: string[] = [];
  for await (const line of readLines(streamOf(chunks))) lines.push(line);
  return lines;
}

test("splits lines across chunk boundaries and trims CR", async () => {
  expect(await collect(["one\ntw", "o\r\nthree"])).toEqual(["one", "two", "three"]);
});

test("keeps empty lines and drops nothing at the end", async () => {
  expect(await collect(["a\n\nb\n"])).toEqual(["a", "", "b"]);
  expect(await collect([])).toEqual([]);
  expect(await collect(["\n"])).toEqual([""]);
});

test("rejoins multi-byte characters split across chunks", async () => {
  const encoded = new TextEncoder().encode("é日\n");
  expect(await collect([encoded.subarray(0, 1), encoded.subarray(1, 4), encoded.subarray(4)]))
    .toEqual(["é日"]);
});

test("counts bytes, not code units, against the 64 MiB line limit", async () => {
  // 24 MiB of three-byte characters is 8 Mi code units: under the limit only
  // when the guard measures UTF-8 bytes rather than string length.
  const chunk = "日".repeat(1024 * 1024);
  await expect(collect([chunk, chunk, chunk, "\n"])).resolves.toHaveLength(1);
  await expect(collect(Array.from({ length: 23 }, () => chunk))).rejects.toThrow("64 MiB");
});

test("reads a long single line in linear time", async () => {
  const megabyte = "x".repeat(1024 * 1024);
  const time = async (chunks: number): Promise<number> => {
    const started = Bun.nanoseconds();
    await collect([...Array.from({ length: chunks }, () => megabyte), "\n"]);
    return Bun.nanoseconds() - started;
  };
  await time(4); // warm up
  const small = await time(4);
  const large = await time(32);
  // Quadratic buffering made the 8x larger line cost ~45x more.
  expect(large).toBeLessThan(small * 24);
});
