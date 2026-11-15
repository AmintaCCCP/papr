import { defineConfig } from "vitest/config";

// Vitest covers the dependency-free, pure logic modules: the browser-extension
// feed detection (feature F6) and the highlight re-anchoring algorithm
// (feature F7, `src/lib/anchor.ts`). These run in a plain node environment.
// Hook tests (`src/hooks/*.test.ts`) and targeted component-integration tests
// (`src/components/*.test.tsx`) execute React effects and opt into jsdom per
// file via the `@vitest-environment jsdom` docblock.
export default defineConfig({
  test: {
    include: [
      "extension/test/**/*.test.{js,ts}",
      "src/lib/**/*.test.ts",
      "src/hooks/**/*.test.ts",
      "src/components/**/*.test.tsx",
    ],
    environment: "node",
  },
});
