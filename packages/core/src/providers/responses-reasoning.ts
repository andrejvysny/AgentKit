import { object, responsesError } from "./responses-types.js";

/** Only public text/summary fields enter the event log; encrypted state never does. */
export class ResponsesReasoning {
  private readonly parts = new Map<string, string>();
  private pending = "";

  accept(event: Record<string, unknown>): void {
    const type = String(event.type);
    const summary = type.includes("summary");
    const index = summary ? event.summary_index : event.content_index;
    const key = `${String(event.item_id)}:${summary ? "summary" : "content"}:${String(index)}`;
    const before = this.parts.get(key) ?? "";
    if (type.endsWith(".delta")) {
      const delta = String(event.delta);
      this.parts.set(key, before + delta);
      this.pending += delta;
    } else {
      const text = type.includes("_part.")
        ? object(event.part).text
        : event.text;
      if (typeof text !== "string" || !text.startsWith(before))
        responsesError("stream_output_mismatch");
      this.parts.set(key, text);
      this.pending += text.slice(before.length);
    }
  }

  takeDelta(): string {
    const delta = this.pending;
    this.pending = "";
    return delta;
  }

  finish(output: Record<string, unknown>[]): string {
    for (const item of output) {
      if (item.type !== "reasoning") continue;
      for (const channel of ["summary", "content"] as const) {
        const parts = item[channel];
        if (!Array.isArray(parts)) continue;
        parts.forEach((value: unknown, index: number) => {
          const part = object(value);
          if (typeof part.text !== "string") return;
          const key = `${String(item.id)}:${channel}:${index}`;
          if (!part.text.startsWith(this.parts.get(key) ?? ""))
            responsesError("stream_output_mismatch");
          this.parts.set(key, part.text);
        });
      }
    }
    return [...this.parts.values()].join("");
  }
}
