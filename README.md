# Testudo

GUI test runner that controls, monitors, and reports on multiple concurrent test suites.

Initial targets: Playwright and Storybook. Planned: Vitest, Jest, Rstest, then non-JS runners (Rust, Go).

Core goals:
- Live view of browser sessions as tests run
- Screen recording, network traffic, and storage capture per session
- Dynamic panes that appear/disappear as suites start and finish
- Coordinate multiple suites running simultaneously
