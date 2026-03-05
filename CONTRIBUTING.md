# Contributing to kube-election

## Getting Started

```bash
# Clone the repository
git clone https://github.com/ctxswitch/kube-election
cd kube-election

# Run tests
cargo test --all
```

## Development

### Running Tests

```bash
# Run library tests
cargo test --lib

# Run doc tests
cargo test --doc

# Run all tests
cargo test --all
```

### Code Style

- Run `cargo fmt` before committing
- Run `cargo clippy` and address warnings
- Follow existing code patterns
- Keep examples concise and focused

### Documentation

- Add doc comments for public APIs
- Update README.md if adding new features
- Add examples for significant features
- Keep documentation concise

## Submitting Changes

1. **Fork the repository**

2. **Create a feature branch**
   ```bash
   git checkout -b feature/your-feature-name
   ```

3. **Make your changes**
   - Write tests for new functionality
   - Ensure all tests pass
   - Update documentation

4. **Commit your changes**

5. **Push to your fork**

6. **Create a Pull Request**
   - Provide a clear description of the changes
   - Reference any related issues
   - Ensure CI passes

## Pull Request Guidelines

- Keep PRs focused on a single feature or fix
- Include tests for new functionality
- Update documentation as needed
- Follow the existing code style
- Ensure all tests pass locally before submitting

## Questions?

Open an issue for:
- Feature requests
- Bug reports
- Questions about implementation
- Clarifications on expected behavior

## License

By contributing, you agree that your contributions will be licensed under the Apache License 2.0.
