#!/usr/bin/env python3
"""Plot RPS and p99 latency comparison from bench CSV files."""
import csv
import sys
import os

try:
    import matplotlib
    matplotlib.use('Agg')
    import matplotlib.pyplot as plt
except ImportError:
    print("pip install matplotlib")
    sys.exit(1)

def load_csv(path):
    elapsed, rps, p99 = [], [], []
    with open(path) as f:
        reader = csv.reader(f)
        header = next(reader, None)
        if header and header[0].startswith('elapsed'):
            pass  # skip header
        else:
            f.seek(0)
            reader = csv.reader(f)
        for row in reader:
            if len(row) < 3:
                continue
            try:
                elapsed.append(float(row[0]))
                rps.append(int(row[1]))
                p99.append(float(row[2]))
            except ValueError:
                continue
    return elapsed, rps, p99

def main():
    results_dir = sys.argv[1] if len(sys.argv) > 1 else 'bench-results'

    scenarios = {}
    for fname in sorted(os.listdir(results_dir)):
        if fname.endswith('.csv'):
            label = fname.replace('.csv', '').replace('_', ' ')
            scenarios[label] = load_csv(os.path.join(results_dir, fname))

    if not scenarios:
        print("No CSV files found")
        sys.exit(1)

    fig, (ax1, ax2) = plt.subplots(2, 1, figsize=(10, 8), sharex=True)

    for label, (elapsed, rps, p99) in scenarios.items():
        ax1.plot(elapsed, rps, label=label, linewidth=1.5)
        ax2.plot(elapsed, p99, label=label, linewidth=1.5)

    ax1.set_ylabel('Requests per second')
    ax1.set_title('RPS over time')
    ax1.legend()
    ax1.grid(True, alpha=0.3)

    ax2.set_ylabel('p99 latency (ms)')
    ax2.set_xlabel('Elapsed time (s)')
    ax2.set_title('p99 latency over time')
    ax2.legend()
    ax2.grid(True, alpha=0.3)

    plt.tight_layout()
    out = os.path.join(results_dir, 'comparison.png')
    plt.savefig(out, dpi=150)
    print(f"Saved {out}")

if __name__ == '__main__':
    main()
