/**
 * Fail a test and exit with a failure code.
 * @param {string} msg - The message to display.
 */
function fail(msg) {
  console.log(`✗ ${msg}`);
  process.exit(1);
}

/**
 * Formally pass a test.
 * @param {string} msg - The message to display.
 */
function pass(msg) {
  console.log(`✓ ${msg}`);
}
