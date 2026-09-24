# Ratatui Design Prompt

Please act as an expert TUI/UX Designer and Rust developer specializing in the `ratatui` ecosystem. Treat the following as a strict design specification for our new interface. 

Our application requires a highly legible, centered workflow with a modern, high-contrast dark aesthetic. Implement the visual hierarchy using `ratatui` components as follows:

1. Layout & Grid (The Centered Workflow):
   - Use `ratatui::layout::Layout` to divide the screen horizontally.
   - Allocate two vertical side panels (left and right), each taking up exactly `Constraint::Percentage(8)` or `Constraint::Length(x)` to act as dual-column status/info panels.
   - The central area must accommodate the main content and markdown tables. Pad the central area visually using layout constraints rather than whitespace to ensure it remains centered.

2. Borders & Structure:
   - Frame the panels using `Block::default().borders(Borders::ALL)`.
   - To achieve a sleek aesthetic, use `BorderType::Rounded` (or `BorderType::Thick` for active panels).
   - Style the borders with a subdued color (e.g., `Color::DarkGray` or a specific RGB like `Color::Rgb(60, 60, 60)`) so they don't distract from the text.

3. Aesthetics & Colors (The "Dark Theme"):
   - Limit the background to a dark canvas (e.g., `Color::Rgb(26, 26, 26)`).
   - Ensure a minimum 4:1 contrast ratio by using crisp foreground text (`Color::White` or `Color::Rgb(255, 255, 255)`).
   - Use `Color::Cyan` strictly for status bar indicators, active state highlights, or key metadata.
   - Instead of shadows or opacity (which are impossible in terminals), use `Modifier::DIM` on secondary text to create a sense of depth and visual hierarchy. Use `Modifier::BOLD` for primary headers.

4. Alignment:
   - Ensure all text within the central content `Paragraph` widgets utilizes `Alignment::Center` where appropriate, while keeping standard markdown tables aligned cleanly.

Provide the exact Rust/`ratatui` code for building this `Layout`, configuring the `Block`s, and setting up the `Style`s to achieve this exact aesthetic.
