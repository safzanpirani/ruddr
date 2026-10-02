// Fetch helpers. Mutations carry the custom header the server requires.

export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
  }
}

async function parse<T>(response: Response): Promise<T> {
  const text = await response.text();
  let body: unknown;
  try {
    body = text ? JSON.parse(text) : {};
  } catch {
    body = { error: text };
  }
  if (!response.ok) {
    const message = (body as { error?: string }).error || `Request failed (${response.status})`;
    throw new ApiError(message, response.status);
  }
  return body as T;
}

export async function get<T>(path: string): Promise<T> {
  return parse<T>(await fetch(path, { credentials: "same-origin", cache: "no-store" }));
}

export async function post<T>(path: string, body: unknown): Promise<T> {
  return parse<T>(
    await fetch(path, {
      method: "POST",
      credentials: "same-origin",
      headers: { "content-type": "application/json", "x-ruddr-request": "1" },
      body: JSON.stringify(body),
    }),
  );
}
