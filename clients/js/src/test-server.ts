import { createServer } from "node:http";
import type { AddressInfo } from "node:net";

export interface RecordedRequest {
  method: string;
  url: string;
  authorization: string | undefined;
  body: string;
}

export interface ScriptedResponse {
  status: number;
  body: unknown;
}

export interface ScriptedServer {
  url: string;
  requests: RecordedRequest[];
  close: () => Promise<void>;
}

/** One-shot HTTP server that answers each request with the next scripted response. */
export async function startScriptedServer(script: ScriptedResponse[]): Promise<ScriptedServer> {
  const remaining = [...script];
  const requests: RecordedRequest[] = [];
  const server = createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on("data", (chunk: Buffer) => chunks.push(chunk));
    req.on("end", () => {
      requests.push({
        method: req.method ?? "",
        url: req.url ?? "",
        authorization: req.headers.authorization,
        body: Buffer.concat(chunks).toString("utf8"),
      });
      const step = remaining.shift() ?? {
        status: 500,
        body: {
          error: { code: "script_exhausted", message: "scripted server ran out of responses" },
        },
      };
      res.statusCode = step.status;
      res.setHeader("content-type", "application/json");
      res.end(JSON.stringify(step.body));
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address() as AddressInfo;
  return {
    url: `http://127.0.0.1:${String(port)}`,
    requests,
    close: () =>
      new Promise((resolve, reject) => {
        server.close((error) => (error === undefined ? resolve() : reject(error)));
      }),
  };
}
