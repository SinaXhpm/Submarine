// Tiny SVG sparkline. We only ever draw ~60 points so a real chart library
// would be massive overkill. Auto-scales to the data range with a small
// floor so a flat line at 50% still has visible padding above/below.

interface SparklineProps {
  data: number[];
  /// Internal viewBox width — does NOT pin the rendered pixel width unless
  /// `fixedWidth` is true. With fixedWidth=false (default) the SVG stretches
  /// to fill its parent and the viewBox just defines the path coordinate
  /// space. This keeps the same component usable in tiny inline cells and
  /// in big detail charts without duplicating it.
  width?: number;
  height?: number;
  color?: string;
  min?: number;
  max?: number;
  fill?: boolean;
  /// Set true to render at exactly `width × height` pixels (legacy use).
  /// Default false → responsive via 100% width.
  fixedWidth?: boolean;
}

const Sparkline = ({
  data,
  width = 160,
  height = 36,
  color = "rgb(16,185,129)",
  min,
  max,
  fill = true,
  fixedWidth = false,
}: SparklineProps) => {
  const svgProps: any = {
    viewBox: `0 0 ${width} ${height}`,
    preserveAspectRatio: "none",
    className: "block",
    height,
  };
  if (fixedWidth) {
    svgProps.width = width;
  } else {
    svgProps.width = "100%";
  }

  if (!data || data.length < 2) {
    return (
      <svg {...svgProps}>
        <line x1="0" y1={height - 1} x2={width} y2={height - 1} className="stroke-white/[0.08]" strokeWidth="1" />
      </svg>
    );
  }

  const observedMin = Math.min(...data);
  const observedMax = Math.max(...data);
  const lo = min ?? observedMin;
  const hi = max ?? Math.max(observedMax, lo + 1);
  const range = hi - lo || 1;

  const padY = 2;
  const usableH = height - padY * 2;
  const stepX = width / (data.length - 1);

  const points = data
    .map((v, i) => {
      const x = i * stepX;
      const y = padY + (1 - (v - lo) / range) * usableH;
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(" ");

  const areaPath = `M0,${height} L${points.replace(/ /g, " L")} L${width},${height} Z`;

  // preserveAspectRatio="none" lets the path stretch horizontally with the
  // container while keeping the height pinned via the parent's CSS height.
  // The vertical stretch is bounded by `viewBox` height which we kept the
  // same as `height`, so the stroke width stays visually consistent.
  return (
    <svg {...svgProps}>
      {fill && <path d={areaPath} fill={color} opacity="0.14" stroke="none" />}
      <polyline
        points={points}
        fill="none"
        stroke={color}
        strokeWidth="1.4"
        strokeLinejoin="round"
        strokeLinecap="round"
        vectorEffect="non-scaling-stroke"
      />
    </svg>
  );
};

export default Sparkline;
