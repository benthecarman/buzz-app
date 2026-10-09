import { expect, it } from "vitest";
import {
  messageNotificationText,
  messagePreview,
  plainText,
  pluginNotificationText,
} from "./content";

const message = {
  channelId: "room",
  messageId: "b".repeat(64),
  authorId: "a".repeat(64),
  createdAt: 1,
  previewContent: "**Hello** [Wes](https://example.com/private)",
};
it.each([
  ["mention", "Pinky mentioned you in #new-notifications"],
  ["thread", "Pinky replied in #new-notifications"],
  ["direct", "Pinky sent you a direct message"],
] as const)(
  "formats %s with sender, conversation and plain preview",
  (category, title) => {
    expect(
      messageNotificationText(
        message,
        category,
        { id: "room", name: "new-notifications" },
        { name: "Pinky" },
      ),
    ).toEqual({ title, body: "Hello Wes" });
  },
);
it("DM mentions do not expose an internal DM name; missing names use key fragments", () => {
  expect(
    messageNotificationText(
      message,
      "mention",
      { id: "room", name: "internal-id", channelType: "dm" },
      undefined,
    ).title,
  ).toBe("aaaaaaaaaa mentioned you in a direct message");
  expect(
    messageNotificationText(message, "thread", undefined, { name: " " }).title,
  ).toBe("aaaaaaaaaa replied in #room");
});
it("flattens blocks, links, images, escapes and code without URLs", () => {
  expect(
    messagePreview(
      "# Heading\n\n**hello** [world](https://example.com)\n\n- one\n- two\n\n`a_b` ![private alt](https://example.com/image)\n\n<div>shown</div>",
    ),
  ).toBe("Heading hello world one two a_b [Image] <div>shown</div>");
  expect(
    messagePreview("[label][ref]\n\n[ref]: https://example.com/private"),
  ).toBe("label");
  expect(
    messagePreview("\\*literal\\* &amp; `code`\n\n```js\nconst a = 1;\n```"),
  ).toBe("*literal* & code const a = 1;");
});
it("bounds source, Unicode output and nesting, with a nonempty fallback", () => {
  expect(messagePreview("😀".repeat(400))).toBe(`${"😀".repeat(199)}…`);
  expect(messagePreview(`${" ".repeat(4096)}not parsed`)).toBe("New message");
  expect(messagePreview(`${"> ".repeat(120)}deep`)).toBe("New message");
  expect(messagePreview("<harness stuff goes here>")).toBe(
    "<harness stuff goes here>",
  );
  expect(messagePreview("<!-- buzz:wave:v1 -->\nLogan waved at you.")).toBe(
    "Logan waved at you.",
  );
  expect(messagePreview("<!-- buzz:wave:v1 -->")).toBe("New message");
});
it("normalizes whitespace/control characters in names and body", () => {
  const text = messageNotificationText(
    { ...message, previewContent: "hello\u202E\nworld" },
    "mention",
    { id: "room", name: "room\nname" },
    { name: "Pinky\u202E\nMouse" },
  );
  expect(text).toEqual({
    title: "Pinky Mouse mentioned you in #room name",
    body: "hello world",
  });
  expect(
    messageNotificationText(
      message,
      "mention",
      { id: "room", name: "y".repeat(200) },
      { name: "x".repeat(200) },
    ).title,
  ).toBe(`${"x".repeat(63)}… mentioned you in #${"y".repeat(63)}…`);
});

it("labels workflow ownership without making the owner the sender, preserving name sanitization", () => {
  const workflow = { ...message, workflowOwnerId: "b".repeat(64) };
  expect(
    messageNotificationText(workflow, "mention", undefined, {
      name: "Wes\u202E\nOwner",
    }).title,
  ).toBe("Workflow · owned by Wes Owner mentioned you in #room");
  expect(
    messageNotificationText(workflow, "thread", undefined, undefined).title,
  ).toBe("Workflow · owned by bbbbbbbbbb replied in #room");
});

it("cuts the source on a whole code point when an emoji straddles the cap", () => {
  // A stripped link leaves room in the 200-point preview for the emoji at
  // UTF-16 unit 4095, whose pair the 4096-unit cut would split.
  const link = `[a](https://example.com/${"x".repeat(4000)})`;
  const text = plainText(`${link}${" ".repeat(4095 - link.length)}😀`);
  expect(() => encodeURIComponent(text)).not.toThrow();
  expect(text).toBe("a");
  const whole = plainText(`${link}${" ".repeat(4094 - link.length)}😀`);
  expect(whole).toBe("a 😀");
});

it("replaces lone surrogates already in the text and keeps valid pairs", () => {
  expect(plainText("a \ud83d b")).toBe("a \uFFFD b");
  expect(plainText("a \ude00 b")).toBe("a \uFFFD b");
  expect(plainText("a \ude00\ud83d")).toBe("a \uFFFD\uFFFD");
  expect(plainText("a 😀 b")).toBe("a 😀 b");
  // At the source cut, after a stripped link leaves room in the preview.
  const link = `[a](https://example.com/${"x".repeat(4000)})`;
  const space = (end: number) => " ".repeat(end - link.length);
  expect(plainText(`${link}${space(4095)}\ude00`)).toBe("a \uFFFD");
  expect(plainText(`${link}${space(4094)}\ud83dz`)).toBe("a \uFFFDz");
  expect(plainText(`${link}${space(4093)}😀z`)).toBe("a 😀z");
  expect(pluginNotificationText({ title: "t \ud83d" }, "Reminders").title).toBe(
    "t \uFFFD",
  );
});
