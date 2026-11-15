import { defineConfig } from "vitest/config";

// Vitest covers the dependency-free, pure logic modules: the browser-extension
// feed detection (feature F6) and the highlight re-anchoring algorithm
// (feature F7, `src/lib/anchor.ts`). These run in a plain node environment.
// Hook tests (`src/hooks/*.test.ts`) execute React effects and opt into jsdom
// per file via the `@vitest-environment jsdom` docblock; full React component
// files stay excluded.
export default defineConfig({
  test: {
    include: [
      "extension/test/**/*.test.{js,ts}",
      "src/lib/**/*.test.ts",
      "src/hooks/**/*.test.ts",
    ],
    environment: "node",
  },
});
