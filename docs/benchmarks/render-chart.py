import json
from pathlib import Path
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
from matplotlib.ticker import MaxNLocator

root = Path(__file__).resolve().parents[2]
data = json.loads((root / 'docs/benchmarks/search-comparison.json').read_text(encoding='utf-8-sig'))
rows = data['measurements']
labels_by_method = {
    'supersearch_scan': 'SuperSearch  /  live scan (NTFS MFT)',
    'windows_powershell_recursive': 'Windows PowerShell  /  recursive walk',
}
labels = [labels_by_method[row['method']] for row in rows]
colors = ['#e8aa42' if row['method'].startswith('supersearch') else '#87949f' for row in rows]
medians = [row['median_ms'] / 1000 for row in rows]
low = [(row['median_ms'] - row['minimum_ms']) / 1000 for row in rows]
high = [(row['maximum_ms'] - row['median_ms']) / 1000 for row in rows]
plt.rcParams.update({'font.family': 'DejaVu Sans', 'font.size': 12})
fig, ax = plt.subplots(figsize=(14, 5.4), facecolor='#17191c')
ax.set_facecolor('#17191c')
fig.subplots_adjust(left=0.41, right=0.91, bottom=0.30, top=0.73)
root_label = data['root'].rstrip('\\/')
fig.text(0.055, 0.91, f'Full {root_label} drive filename search', color='#f4f4f4', fontsize=23, weight='bold')
fig.text(0.055, 0.845, "notepad.exe  |  Complete recursive searches  |  Lower is faster", color='#b5bdc5', fontsize=12)
ax.barh(range(len(rows)), medians, height=0.46, color=colors, edgecolor='none', zorder=3)
if sum(row['samples'] for row in rows) > len(rows):
    ax.errorbar(medians, range(len(rows)), xerr=[low, high], fmt='none', ecolor='#e3e7eb', elinewidth=1.1, capsize=3, zorder=4)
max_seconds = max(row['maximum_ms'] for row in rows) / 1000
for i, (value, row) in enumerate(zip(medians, rows)):
    ax.text(row['maximum_ms'] / 1000 + max_seconds * 0.025, i, f'{value:.2f} s', va='center', color='#f2f3f4', fontsize=13, weight='bold')
ax.set_yticks(range(len(rows)), labels, color='#e4e7ea', fontsize=12)
ax.invert_yaxis()
ax.set_xlim(0, max_seconds * 1.24)
ax.xaxis.set_major_locator(MaxNLocator(6))
ax.set_xlabel('Elapsed time (seconds)', color='#aeb8c1', labelpad=10, fontsize=10)
ax.tick_params(axis='x', colors='#9ba7b2', labelsize=10, length=0)
ax.tick_params(axis='y', length=0, pad=18)
ax.grid(axis='x', color='#333b43', linewidth=0.7, zorder=0)
for spine in ax.spines.values():
    spine.set_visible(False)
cov = data['coverage']
fig.text(0.055, 0.15, f"Full drive scope: {data['root']}  |  SuperSearch {cov['supersearch_matches']} files via MFT vs walk {cov['baseline_matches']} paths ({cov['baseline_only_paths']} hardlink aliases).", color='#adb6bf', fontsize=10)
fig.text(0.055, 0.10, 'Same filename query; both skip junctions. No indexes. Startup included. One complete pass each; SuperSearch ran first; caches were not flushed.', color='#8e9aa6', fontsize=9)
privilege_note = 'Elevated run.' if data['elevated'] else 'Current-user access; protected locations excluded. NTFS MFT acceleration unavailable.'
fig.text(0.055, 0.05, f"{data['date']}  |  {privilege_note} Baseline: Windows PowerShell 5.1; Explorer UI is excluded.", color='#8e9aa6', fontsize=9)
out = root / 'docs/benchmarks/search-comparison.png'
fig.savefig(out, dpi=160, facecolor=fig.get_facecolor())
print(f'Created {out.name}')
