// Template-set cases about context size and image input.
import {
  fail,
  filler,
  httpFail,
  ok,
  type ProbeCase,
  singleCall,
  user,
  weather,
  weatherSchema,
} from "./cases";
import { parseArgs } from "./lib";

const RED_PNG_64 =
  "iVBORw0KGgoAAAANSUhEUgAAAEAAAABACAIAAAAlC+aJAAAAgElEQVR4nO3RwQkAMAwDMe+/dDtEH6Jw4AES3c729fwFPaAJK6AJK6AJK6AJK6AJK6AJK6AJK6AJK6AJK6AJK6AJK6AJK6AJK6AJK6AJK6AJK6AJK6AJK6AJK6AJK6AJK6AJK6AJK6AJK6AJK6AJK6AJK6AJK6AJK6AJKzCv+LILqdzw4i4ZAl4AAAAASUVORK5CYII=";

const imageCase: ProbeCase = {
  id: "image-input",
  what: "an image_url data URL (64x64 red PNG) is accepted and seen",
  request: {
    messages: [
      {
        role: "user",
        content: [
          { type: "text", text: "What colour is this square? One word." },
          {
            type: "image_url",
            image_url: { url: `data:image/png;base64,${RED_PNG_64}` },
          },
        ],
      },
    ],
    max_tokens: 4000,
  },
  check: (c) => {
    if (c.status !== 200) return httpFail(c);
    const detail = `content=${c.content.slice(0, 80)}`;
    return /red/i.test(c.content) ? ok(detail) : fail(detail);
  },
  repeats: 1,
};

function longToolCase(maxModelLen: number | null): ProbeCase {
  const tooSmall = maxModelLen === null || maxModelLen < 140_000;
  return {
    id: "tool-after-120k-context",
    what: "valid tool call after ~120k tokens of earlier context",
    request: {
      messages: [
        user(
          `${filler(120_000, "Log note: the owner lives in Trnava.", 0.3)}\n\nUsing the log above, get the weather for the city where the owner lives.`,
        ),
      ],
      tools: [weather],
      max_tokens: 8000,
    },
    check: (c) => {
      const base = singleCall(c, "get_weather", weatherSchema);
      const city = String(
        (c.toolCalls[0] ? parseArgs(c.toolCalls[0]) : null)?.city,
      );
      if (!base.pass) return base;
      return city === "Trnava" ? ok(`city=${city}`) : fail(`city=${city}`);
    },
    modes: ["nonstream"],
    repeats: 1,
    ...(tooSmall
      ? { skip: `max_model_len=${maxModelLen ?? "unknown"} < 140000` }
      : {}),
  };
}

function overContextCase(maxModelLen: number | null): ProbeCase {
  return {
    id: "over-context-rejected",
    what: "a prompt over max_model_len is rejected with 400 that names the limit",
    request: {
      messages: [
        user(filler((maxModelLen ?? 0) + 20_000, "Log note: nothing.", 0.5)),
      ],
      max_tokens: 100,
    },
    check: (c) => {
      const detail = `http ${c.status}: ${(c.errorBody ?? "").slice(0, 200)}`;
      return c.status === 400 && /context|token|length/i.test(c.errorBody ?? "")
        ? ok(detail)
        : fail(detail);
    },
    modes: ["nonstream"],
    repeats: 1,
    ...(maxModelLen === null ? { skip: "max_model_len unknown" } : {}),
  };
}

export const templateContextCases = (
  maxModelLen: number | null,
): ProbeCase[] => [
  longToolCase(maxModelLen),
  overContextCase(maxModelLen),
  imageCase,
];
