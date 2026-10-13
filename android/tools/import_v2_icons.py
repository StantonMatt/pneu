#!/usr/bin/env python3
"""Import the approved SVG paths as Android vectors; never rasterize the mark."""
from pathlib import Path
import hashlib
import xml.etree.ElementTree as ET

ANDROID = 'http://schemas.android.com/apk/res/android'
AAPT = 'http://schemas.android.com/aapt'
SVG = 'http://www.w3.org/2000/svg'
ET.register_namespace('android', ANDROID)
ET.register_namespace('aapt', AAPT)
ROOT = Path(__file__).resolve().parents[2]
ASSETS = ROOT / 'docs/design/v2/assets'
DRAWABLE = ROOT / 'android/app/src/main/res/drawable'
SOURCES = {
    'ic_launcher_background.svg': 'ic_launcher_background.xml',
    'ic_launcher_foreground.svg': 'ic_launcher_foreground.xml',
    'ic_launcher_monochrome.svg': 'ic_launcher_monochrome.xml',
    'ic_stat_pneu.svg': 'ic_stat_pneu.xml',
    'pneu-mark.svg': 'ic_pneu_mark.xml',
    'ic_launcher_preview.svg': 'ic_launcher_preview.xml',
}


def attr(name):
    return f'{{{ANDROID}}}{name}'


def convert(source, crop=False):
    svg = ET.fromstring(source)
    x, y, width, height = map(float, svg.attrib['viewBox'].split())
    if x or y:
        raise ValueError('A translated SVG viewport needs an explicit group')
    if crop:
        if (width, height) != (108, 108):
            raise ValueError('The in-app crop requires the 108-unit launcher preview')
        width = height = 72
    vector = ET.Element('vector', {
        attr('width'): f'{width:g}dp', attr('height'): f'{height:g}dp',
        attr('viewportWidth'): f'{width:g}', attr('viewportHeight'): f'{height:g}',
    })
    gradients = {g.attrib['id']: g for g in svg.iter() if g.tag in {f'{{{SVG}}}linearGradient', f'{{{SVG}}}radialGradient'}}
    paths = list(svg.findall(f'{{{SVG}}}path'))
    if not paths:
        raise ValueError('The approved SVG must contain paths')
    allowed = {f'{{{SVG}}}path', f'{{{SVG}}}defs'}
    if any(child.tag not in allowed for child in svg):
        raise ValueError('Unsupported SVG geometry; preserve it explicitly before importing')
    parent = ET.SubElement(vector, 'group', {attr('translateX'): '-18', attr('translateY'): '-18'}) if crop else vector
    for path in paths:
        p = ET.SubElement(parent, 'path', {attr('pathData'): path.attrib['d']})
        for svg_name, android_name in [('fill', 'fillColor'), ('stroke', 'strokeColor')]:
            color = path.attrib.get(svg_name, '#000000' if svg_name == 'fill' else 'none')
            if color.startswith('url(#'):
                gradient = gradients[color[5:-1]]
                if gradient.attrib.get('gradientUnits') != 'userSpaceOnUse':
                    raise ValueError('Only absolute-coordinate gradients are supported')
                wrapper = ET.SubElement(p, f'{{{AAPT}}}attr', {'name': f'android:{android_name}'})
                radial = gradient.tag.endswith('radialGradient')
                g = ET.SubElement(wrapper, 'gradient', {attr('type'): 'radial' if radial else 'linear'})
                coordinates = [('cx', 'centerX'), ('cy', 'centerY'), ('r', 'gradientRadius')] if radial else [('x1', 'startX'), ('y1', 'startY'), ('x2', 'endX'), ('y2', 'endY')]
                for svg_coordinate, android_coordinate in coordinates:
                    g.set(attr(android_coordinate), gradient.attrib[svg_coordinate])
                for stop in gradient:
                    rgba = stop.attrib['stop-color']
                    opacity = float(stop.attrib.get('stop-opacity', '1'))
                    if opacity < 1:
                        rgba = f'#{round(opacity * 255):02X}' + rgba.lstrip('#')
                    ET.SubElement(g, 'item', {
                        attr('offset'): stop.attrib['offset'], attr('color'): rgba,
                    })
            else:
                p.set(attr(android_name), '#00000000' if color == 'none' else color)
        for svg_name, android_name in [('stroke-width', 'strokeWidth'), ('stroke-linecap', 'strokeLineCap'),
                                       ('stroke-linejoin', 'strokeLineJoin'), ('fill-opacity', 'fillAlpha'),
                                       ('stroke-opacity', 'strokeAlpha')]:
            if svg_name in path.attrib:
                p.set(attr(android_name), path.attrib[svg_name])
    ET.indent(vector, space='    ')
    return ET.tostring(vector, encoding='unicode')


def main():
    # Validate the complete snapshot before replacing any resource.
    snapshot = {name: (ASSETS / name).read_bytes() for name in SOURCES}
    converted = {name: convert(source) for name, source in snapshot.items()}
    cropped = convert(snapshot['ic_launcher_preview.svg'], crop=True)
    outputs = [(name, destination, converted[name]) for name, destination in SOURCES.items()]
    outputs.append(('ic_launcher_preview.svg', 'ic_launcher_preview_crop.xml', cropped))
    for name, destination, geometry in outputs:
        source = snapshot[name]
        digest = hashlib.sha256(source).hexdigest()
        output = '<?xml version="1.0" encoding="utf-8"?>\n'
        output += f'<!-- Generated from docs/design/v2/assets/{name}; SHA-256 {digest}. -->\n'
        output += geometry + '\n'
        (DRAWABLE / destination).write_text(output)
        print(f'{name}: {digest}')


if __name__ == '__main__':
    main()
