import { defineConfig } from 'vitest/config';
import cesium from 'vite-plugin-cesium';

export default defineConfig({
  plugins: [cesium()],
  test: {
    environment: 'jsdom',
    include: ['tests/unit/**/*.test.js'],
    globals: true,
  },
});
