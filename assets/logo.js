(() => {
  'use strict';
  const reduced = matchMedia('(prefers-reduced-motion: reduce)');
  const vertex = 'attribute vec2 a; varying vec2 uv; void main(){uv=a*.5+.5;gl_Position=vec4(a,0.,1.);}';
  const fragment = `
    precision mediump float;
    varying vec2 uv;
    uniform sampler2D mask;
    uniform vec2 size;
    uniform float dpr;
    uniform float light;
    uniform float illustration;
    void main(){
      vec2 p=uv*size;
      float angle=.32;
      mat2 turn=mat2(cos(angle),-sin(angle),sin(angle),cos(angle));
      float pitch=mix(1.65,3.4,illustration);
      vec2 cell=fract(turn*p/pitch)-.5;
      float shade=clamp(.5+.37*sin(uv.x*3.8-uv.y*2.1+light*.65),0.,1.);
      float radius=mix(mix(.32,.23,illustration),.7,shade);
      float aa=.65/(pitch*dpr);
      float dots=1.-smoothstep(radius-aa,radius+aa,length(cell));
      // The SVG supplies both the clean silhouette and the ink blend.
      // Use the same source as the fallback so only the print screen changes.
      vec4 pigment=texture2D(mask,vec2(uv.x,1.-uv.y));
      float edge=pigment.a;
      vec3 ink=pigment.rgb;
      float density=mix(1.,dots,illustration);
      gl_FragColor=vec4(ink,density*edge);
    }`;
  // Small identity marks use the SVG directly. Their shader has no halftone
  // effect, and replacing the image with a WebGL surface can lose the mark
  // when mobile browsers discard or fail to rasterize that surface.
  for (const logo of document.querySelectorAll('.hero-feather')) {
    const fallback = logo.querySelector('img');
    const canvas = logo.querySelector('canvas');
    const gl = canvas.getContext('webgl', { alpha:true, antialias:true, premultipliedAlpha:false, powerPreference:'low-power' });
    if (!gl) continue;
    let frame=0, current=0, target=0, ready=false, visible=true, lost=false;
    try {
      const program=gl.createProgram();
      for (const [type,source] of [[gl.VERTEX_SHADER,vertex],[gl.FRAGMENT_SHADER,fragment]]) {
        const shader=gl.createShader(type);gl.shaderSource(shader,source);gl.compileShader(shader);
        if(!gl.getShaderParameter(shader,gl.COMPILE_STATUS)) throw new Error('Logo shader compilation failed');
        gl.attachShader(program,shader);gl.deleteShader(shader);
      }
      gl.linkProgram(program);
      if(!gl.getProgramParameter(program,gl.LINK_STATUS)) throw new Error('Logo shader linking failed');
      gl.useProgram(program);
      gl.uniform1f(gl.getUniformLocation(program,'illustration'),logo.classList.contains('hero-feather') ? 1 : 0);
      const buffer=gl.createBuffer();gl.bindBuffer(gl.ARRAY_BUFFER,buffer);
      gl.bufferData(gl.ARRAY_BUFFER,new Float32Array([-1,-1,1,-1,-1,1,-1,1,1,-1,1,1]),gl.STATIC_DRAW);
      const a=gl.getAttribLocation(program,'a');gl.enableVertexAttribArray(a);gl.vertexAttribPointer(a,2,gl.FLOAT,false,0,0);
      const size=gl.getUniformLocation(program,'size'), dpr=gl.getUniformLocation(program,'dpr'), light=gl.getUniformLocation(program,'light');
      const texture=gl.createTexture();gl.bindTexture(gl.TEXTURE_2D,texture);
      gl.texParameteri(gl.TEXTURE_2D,gl.TEXTURE_MIN_FILTER,gl.LINEAR);gl.texParameteri(gl.TEXTURE_2D,gl.TEXTURE_MAG_FILTER,gl.LINEAR);
      gl.texParameteri(gl.TEXTURE_2D,gl.TEXTURE_WRAP_S,gl.CLAMP_TO_EDGE);gl.texParameteri(gl.TEXTURE_2D,gl.TEXTURE_WRAP_T,gl.CLAMP_TO_EDGE);
      function schedule(){if(!frame&&ready&&visible&&!lost&&!document.hidden)frame=requestAnimationFrame(draw);}
      function draw(){
        frame=0;
        if(lost||!visible||document.hidden)return;
        current+=(target-current)*.12;
        gl.uniform1f(light,current);gl.drawArrays(gl.TRIANGLES,0,6);
        logo.classList.add('shader-ready');
        if(Math.abs(current-target)>.002)schedule();
      }
      function resize(){
        const box=logo.getBoundingClientRect(), ratio=Math.min(devicePixelRatio||1,2);
        canvas.width=Math.max(1,Math.round(box.width*ratio));canvas.height=Math.max(1,Math.round(box.height*ratio));
        gl.viewport(0,0,canvas.width,canvas.height);gl.uniform2f(size,box.width,box.height);gl.uniform1f(dpr,ratio);schedule();
      }
      const source=new Image();
      source.onload=()=>{
        if(lost)return;
        gl.bindTexture(gl.TEXTURE_2D,texture);gl.texImage2D(gl.TEXTURE_2D,0,gl.RGBA,gl.RGBA,gl.UNSIGNED_BYTE,source);
        ready=true;resize();
      };
      source.src=fallback.src;
      new ResizeObserver(resize).observe(logo);
      new IntersectionObserver(([entry])=>{visible=entry.isIntersecting;schedule();}).observe(logo);
      const link=logo.closest('a') || logo.closest('.hero');
      function enter(){if(!reduced.matches){target=1;schedule();}}
      function leave(){target=0;schedule();}
      link.addEventListener('pointerenter',enter);link.addEventListener('pointerleave',leave);
      if(logo.classList.contains('hero-feather')) link.addEventListener('pointermove',event=>{
        if(reduced.matches||event.pointerType==='touch')return;
        const box=link.getBoundingClientRect();target=((event.clientX-box.left)/box.width-.5)*2;schedule();
      },{passive:true});
      link.addEventListener('focus',enter);link.addEventListener('blur',leave);
      reduced.addEventListener('change',()=>{target=0;current=0;schedule();});
      document.addEventListener('visibilitychange',schedule);
      canvas.addEventListener('webglcontextlost',event=>{event.preventDefault();lost=true;cancelAnimationFrame(frame);logo.classList.remove('shader-ready');});
    } catch {
      logo.classList.remove('shader-ready');
    }
  }
})();
