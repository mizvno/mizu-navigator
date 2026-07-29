import os
import re

def fix_file(path):
    with open(path, 'r', encoding='utf-8') as f:
        content = f.read()

    # In tests, a lot of VariableStore::new() are passed directly.
    # We replace `let store = VariableStore::new();` with `let store = VariableStore::new().freeze();`
    content = re.sub(
        r'let store = (crate::core::types::)?VariableStore::new\(\);',
        r'let store = \1VariableStore::new().freeze();',
        content
    )

    # For `tab.store.set(` -> `tab.store.set_runtime(`
    content = re.sub(r'tab\.store\s*\.\s*set\(', r'tab.store.set_runtime(', content)

    # In manager.rs: `store: VariableStore::new(),` -> `store: VariableStore::new().freeze(),`
    content = re.sub(
        r'store:\s*(crate::core::types::)?VariableStore::new\(\),',
        r'store: \1VariableStore::new().freeze(),',
        content
    )

    with open(path, 'w', encoding='utf-8') as f:
        f.write(content)

for root, _, files in os.walk('src'):
    for f in files:
        if f.endswith('.rs'):
            fix_file(os.path.join(root, f))
