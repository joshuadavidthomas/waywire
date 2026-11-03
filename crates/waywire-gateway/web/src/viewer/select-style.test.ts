import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const css = await readFile(new URL("./style.css", import.meta.url), "utf8");

function declarations(selector: string): Map<string, string> {
  const escapedSelector = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const match = css.match(new RegExp(`${escapedSelector}\\s*\\{([^}]*)\\}`));
  const body = match?.[1];
  assert.ok(body, `missing ${selector} rule`);

  return new Map(
    body
      .split(";")
      .map((declaration) => declaration.trim())
      .filter(Boolean)
      .map((declaration) => {
        const separator = declaration.indexOf(":");
        return [
          declaration.slice(0, separator).trim(),
          declaration.slice(separator + 1).trim(),
        ];
      }),
  );
}

function contrast(foreground: string, background: string): number {
  const luminance = (hex: string): number => {
    assert.match(hex, /^#[a-f0-9]{6}$/i);
    return [0.2126, 0.7152, 0.0722].reduce((total, weight, index) => {
      const channel =
        Number.parseInt(hex.slice(1 + index * 2, 3 + index * 2), 16) / 255;
      const linear =
        channel <= 0.04045
          ? channel / 12.92
          : ((channel + 0.055) / 1.055) ** 2.4;
      return total + weight * linear;
    }, 0);
  };

  const foregroundLuminance = luminance(foreground);
  const backgroundLuminance = luminance(background);
  return (
    (Math.max(foregroundLuminance, backgroundLuminance) + 0.05) /
    (Math.min(foregroundLuminance, backgroundLuminance) + 0.05)
  );
}

test("native menu options use explicit dark panel colors", () => {
  assert.equal(declarations(".menu-select").get("color-scheme"), "dark");

  const expectedRules = [
    [".menu-select option", "var(--panel)", "var(--ink)"],
    [".menu-select option:checked", "var(--panel-on)", "var(--ink)"],
    [".menu-select option:disabled", "var(--panel)", "var(--ink-dim)"],
    [
      ".menu-select option:checked:disabled",
      "var(--panel-on)",
      "var(--ink-dim)",
    ],
  ] as const;

  for (const [selector, background, color] of expectedRules) {
    const rule = declarations(selector);
    assert.equal(rule.get("background"), background);
    assert.equal(rule.get("color"), color);
  }
});

test("every option state keeps readable contrast", () => {
  const root = declarations(":root");
  const combinations = [
    ["--ink", "--panel"],
    ["--ink", "--panel-on"],
    ["--ink-dim", "--panel"],
    ["--ink-dim", "--panel-on"],
  ] as const;

  for (const [foregroundToken, backgroundToken] of combinations) {
    const foreground = root.get(foregroundToken);
    const background = root.get(backgroundToken);
    assert.ok(foreground && background);
    assert.ok(
      contrast(foreground, background) >= 4.5,
      `${foregroundToken} on ${backgroundToken} must meet 4.5:1 contrast`,
    );
  }
});
