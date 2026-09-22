/// Which grammar a pack file is highlighted with. Routed by filename, because a pack carries no
/// other declaration of what its files are: `workflow.star` is Starlark, which is Python's grammar
/// with a smaller vocabulary, so it rides the `python` tokenizer rather than a hand-rolled one.
export function languageOf(path: string): string {
  const name = path.split('/').pop() ?? path;
  const dot = name.lastIndexOf('.');
  const ext = dot <= 0 ? '' : name.slice(dot + 1).toLowerCase();
  switch (ext) {
    case 'star':
    case 'bzl':
    case 'py':
      return 'python';
    case 'toml':
      return 'toml';
    case 'md':
    case 'markdown':
      return 'markdown';
    case 'json':
    case 'jsonl':
      return 'json';
    case 'sh':
    case 'bash':
      return 'shell';
    case 'yaml':
    case 'yml':
      return 'yaml';
    default:
      return name === 'BUILD' || name === 'WORKSPACE' ? 'python' : 'plaintext';
  }
}
