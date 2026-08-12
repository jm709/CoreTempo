/** A non-2xx answer from the daemon; `code` is the machine key from the error body. */
export class CoreTempoApiError extends Error {
  readonly status: number;
  readonly code: string;

  constructor(status: number, code: string, message: string) {
    super(message);
    this.name = "CoreTempoApiError";
    this.status = status;
    this.code = code;
  }
}

/** The request never got an HTTP answer (refused, DNS, aborted TLS…); `cause` is the original error. */
export class CoreTempoRequestError extends Error {
  readonly url: string;

  constructor(url: string, cause: unknown) {
    super(`request to ${url} failed: ${String(cause)}`, { cause });
    this.name = "CoreTempoRequestError";
    this.url = url;
  }
}
