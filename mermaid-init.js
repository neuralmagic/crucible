// Customized mermaid init: render manually, then attach svg-pan-zoom (with on-screen
// zoom controls) to every diagram so wide flowcharts are navigable inside the content
// column. NOTE: re-running `mdbook-mermaid install` will clobber this file.
(() => {
    const darkThemes = ['ayu', 'navy', 'coal'];
    const lightThemes = ['light', 'rust'];

    const classList = document.getElementsByTagName('html')[0].classList;

    let lastThemeWasLight = true;
    for (const cssClass of classList) {
        if (darkThemes.includes(cssClass)) {
            lastThemeWasLight = false;
            break;
        }
    }

    const theme = lastThemeWasLight ? 'default' : 'dark';
    mermaid.initialize({ startOnLoad: false, theme });

    const attachPanZoom = (svg) => {
        // Give the SVG a fixed viewport so pan/zoom has somewhere to move within.
        svg.style.maxWidth = 'none';
        svg.style.width = '100%';
        svg.style.height = '70vh';
        svgPanZoom(svg, {
            zoomEnabled: true,
            controlIconsEnabled: true,
            fit: true,
            center: true,
            minZoom: 0.3,
            maxZoom: 12,
            zoomScaleSensitivity: 0.3,
        });
    };

    window.addEventListener('load', async () => {
        await mermaid.run({ querySelector: '.mermaid' });
        // One frame so the browser lays the SVGs out before we measure them.
        requestAnimationFrame(() => {
            document.querySelectorAll('.mermaid > svg').forEach(attachPanZoom);
        });
    });

    // Switching between a light and dark theme needs a re-render; reload to pick it up.
    for (const darkTheme of darkThemes) {
        document.getElementById(darkTheme).addEventListener('click', () => {
            if (lastThemeWasLight) {
                window.location.reload();
            }
        });
    }

    for (const lightTheme of lightThemes) {
        document.getElementById(lightTheme).addEventListener('click', () => {
            if (!lastThemeWasLight) {
                window.location.reload();
            }
        });
    }
})();
