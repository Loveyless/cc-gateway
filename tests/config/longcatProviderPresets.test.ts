import { describe, expect, it } from "vitest";
import { claudeDesktopProviderPresets } from "@/config/claudeDesktopProviderPresets";
import { providerPresets } from "@/config/claudeProviderPresets";
import { codexProviderPresets } from "@/config/codexProviderPresets";
const LONGCAT_MODEL = "LongCat-2.0";
const LONGCAT_DISPLAY_NAME = "LongCat 2.0";
const LONGCAT_BRAND = "LongCat";
const FLASH_VARIANT = "Flash";
const CHAT_SUFFIX = "Chat";
const REMOVED_LONGCAT_NAMES = [
  `${LONGCAT_BRAND}-${FLASH_VARIANT}-${CHAT_SUFFIX}`,
  `${LONGCAT_BRAND} ${FLASH_VARIANT} ${CHAT_SUFFIX}`,
  `${LONGCAT_MODEL}-Preview`,
  `${LONGCAT_DISPLAY_NAME} Preview`,
];

function findLongcatPreset<T extends { name: string }>(presets: T[]): T {
  const preset = presets.find((item) => item.name === "Longcat");

  expect(preset).toBeDefined();
  return preset!;
}

describe("Longcat provider presets", () => {
  it("uses the official LongCat 2.0 model for Claude Code", () => {
    const preset = findLongcatPreset(providerPresets);
    const env = (preset.settingsConfig as { env: Record<string, unknown> }).env;

    expect(env).toMatchObject({
      ANTHROPIC_MODEL: LONGCAT_MODEL,
      ANTHROPIC_SMALL_FAST_MODEL: LONGCAT_MODEL,
      ANTHROPIC_DEFAULT_HAIKU_MODEL: LONGCAT_MODEL,
      ANTHROPIC_DEFAULT_SONNET_MODEL: LONGCAT_MODEL,
      ANTHROPIC_DEFAULT_OPUS_MODEL: LONGCAT_MODEL,
      CLAUDE_CODE_MAX_OUTPUT_TOKENS: "131072",
    });
  });

  it("uses the official LongCat 2.0 model for Claude Desktop routes", () => {
    const preset = findLongcatPreset(claudeDesktopProviderPresets);

    expect(
      preset.modelRoutes?.map((route) => ({
        upstreamModel: route.upstreamModel,
        labelOverride: route.labelOverride,
      })),
    ).toEqual([
      {
        upstreamModel: LONGCAT_MODEL,
        labelOverride: LONGCAT_MODEL,
      },
    ]);
  });

  it("uses the official LongCat 2.0 model for Codex", () => {
    const preset = findLongcatPreset(codexProviderPresets);

    expect(preset.config).toContain(`model = "${LONGCAT_MODEL}"`);
    expect(preset.modelCatalog).toEqual([
      expect.objectContaining({
        model: LONGCAT_MODEL,
        displayName: LONGCAT_DISPLAY_NAME,
        contextWindow: 1048576,
      }),
    ]);
  });

  it("does not keep retired or preview Longcat model names", () => {
    const longcatPresets = [
      findLongcatPreset(providerPresets),
      findLongcatPreset(claudeDesktopProviderPresets),
      findLongcatPreset(codexProviderPresets),
    ];
    const serializedPresets = JSON.stringify(longcatPresets);

    for (const removedName of REMOVED_LONGCAT_NAMES) {
      expect(serializedPresets).not.toContain(removedName);
    }
  });
});
