/** @type {import('tailwindcss').Config} */
export default {
  content: ["./index.html", "./src/**/*.{js,ts,jsx,tsx}"],
  theme: {
    extend: {
      colors: {
        background: "var(--background)",
        surface: "#09090b",   
        panel: "#121214",     
        primary: "rgb(var(--primary) / <alpha-value>)",
        border: "var(--border)",    
        muted: "#a1a1aa",
      },
      // Tailwind's spin and pulse, but they stop: 12 turns, 5 pulses. For
      // states that can last (a tab connecting or waiting for an answer, a
      // retry, broadcast on): the browser draws an endless animation at every
      // frame, and a tab left waiting kept 13-29% of a CPU core busy for a
      // dot and a spinner that had nothing new to say.
      animation: {
        "spin-brief": "spin 1s linear 12",
        "pulse-brief": "pulse 2s cubic-bezier(0.4, 0, 0.6, 1) 5",
      },
    },
  },
  plugins: [],
}