(() => {
  'use strict';
  const canvas = document.querySelector('#halftone');
  const fallback = document.querySelector('.art-fallback');
  const hero = document.querySelector('.hero');
  const reducedMotion = matchMedia('(prefers-reduced-motion: reduce)');
  const gl = canvas.getContext('webgl', { alpha: true, antialias: false, premultipliedAlpha: false, preserveDrawingBuffer: false, powerPreference: 'low-power' });
  const showFallback = () => { canvas.hidden = true; canvas.style.display = 'none'; fallback.hidden = false; };
  if (!gl) { showFallback(); return; }

  const vertex = `
    attribute vec2 aPosition;
    varying vec2 vUv;
    void main(){ vUv=aPosition*.5+.5; gl_Position=vec4(aPosition,0.,1.); }
  `;
  const form = `
    precision highp float;
    varying vec2 vUv;
    uniform vec2 uSize;
    uniform vec2 uPointer;
    uniform float uFraming;
    mat2 turn(float a){float c=cos(a),s=sin(a);return mat2(c,-s,s,c);}
    float surface(vec3 p){
      p.xy=turn(-.36 + uPointer.x*.09)*p.xy;
      p.yz=turn(.64 + uPointer.y*.075)*p.yz;
      p.xz=turn(.22)*p.xz;
      float angle=atan(p.z,p.x);
      float radius=1.15+.08*cos(3.*angle+.3);
      vec2 crossSection=vec2(length(p.xz)-radius,p.y-.17*sin(2.*angle));
      crossSection=turn(angle*.5+.72)*crossSection;
      vec2 d=abs(crossSection)-vec2(.44,.052);
      return length(max(d,0.))+min(max(d.x,d.y),0.)-.028;
    }
    vec3 normalAt(vec3 p){
      vec2 e=vec2(.002,0.);
      return normalize(vec3(surface(p+e.xyy)-surface(p-e.xyy),surface(p+e.yxy)-surface(p-e.yxy),surface(p+e.yyx)-surface(p-e.yyx)));
    }
    void main(){
      vec2 uv=(vUv-.5)*2.;
      uv.x*=uSize.x/uSize.y;
      vec3 origin=vec3(0.,0.,5.7);
      float framing=uFraming*max(1.,uSize.y/uSize.x);
      vec3 ray=normalize(vec3(uv*framing,-4.6));
      float b=dot(origin,ray), c=dot(origin,origin)-3.46;
      float discriminant=b*b-c;
      if(discriminant<0.){gl_FragColor=vec4(0.);return;}
      float distance=max(0.,-b-sqrt(discriminant));
      float end=-b+sqrt(discriminant);
      float hit=0.;
      for(int i=0;i<85;i++){
        float step=surface(origin+ray*distance);
        if(step<.0015){hit=1.;break;}
        distance+=max(step*.65,.0005);
        if(distance>end)break;
      }
      if(hit<.5){gl_FragColor=vec4(0.);return;}
      vec3 p=origin+ray*distance;
      vec3 n=normalAt(p);
      vec3 light=normalize(vec3(-.7+uPointer.x*.45,1.1+uPointer.y*.3,1.6));
      float diffuse=max(dot(n,light),0.);
      float ao=clamp(1.-2.5*(.16-surface(p+n*.16)),.45,1.);
      float shade=(.16+.78*diffuse)*ao;
      float coverage=clamp(1.09-shade,.11,1.);
      float darkness=clamp((.64-shade)*1.8,0.,1.);
      gl_FragColor=vec4(coverage,darkness,0.,1.);
    }
  `;
  const print = `
    precision highp float;
    varying vec2 vUv;
    uniform sampler2D uForm;
    uniform vec2 uCssSize;
    uniform float uPitch;
    uniform float uDpr;
    mat2 turn(float a){float c=cos(a),s=sin(a);return mat2(c,-s,s,c);}
    float random(vec2 p){return fract(sin(dot(p,vec2(127.1,311.7)))*43758.5453);}
    void main(){
      vec2 paper=vUv*uCssSize;
      vec2 screen=turn(.26)*paper/uPitch;
      vec2 cell=floor(screen);
      vec2 center=turn(-.26)*((cell+.5)*uPitch)/uCssSize;
      vec4 impression=texture2D(uForm,center);
      float radius=.65*sqrt(impression.r);
      float grain=random(cell);
      radius*=.98+.04*grain;
      float edge=length(fract(screen)-.5)-radius;
      float aa=.7/(uPitch*uDpr);
      float dots=(1.-smoothstep(-aa,aa,edge))*smoothstep(.35,.8,impression.a);
      vec3 vermilion=vec3(.82,.235,.13);
      vec3 shadow=vec3(.31,.12,.083);
      vec3 ink=mix(vermilion,shadow,pow(impression.g,1.5)*.88);
      ink*=.985+.025*grain;
      gl_FragColor=vec4(ink,dots);
    }
  `;
  function program(source) {
    const result = gl.createProgram();
    for (const [type, text] of [[gl.VERTEX_SHADER,vertex],[gl.FRAGMENT_SHADER,source]]) {
      const shader=gl.createShader(type); gl.shaderSource(shader,text); gl.compileShader(shader);
      if(!gl.getShaderParameter(shader,gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(shader));
      gl.attachShader(result,shader); gl.deleteShader(shader);
    }
    gl.linkProgram(result);
    if(!gl.getProgramParameter(result,gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(result));
    return result;
  }
  let formProgram, printProgram;
  try { formProgram=program(form); printProgram=program(print); }
  catch(error) { console.error('Quill artwork:',error); showFallback(); return; }
  const quad=gl.createBuffer();
  gl.bindBuffer(gl.ARRAY_BUFFER,quad);
  gl.bufferData(gl.ARRAY_BUFFER,new Float32Array([-1,-1,1,-1,-1,1,-1,1,1,-1,1,1]),gl.STATIC_DRAW);
  const texture=gl.createTexture();
  gl.bindTexture(gl.TEXTURE_2D,texture);
  gl.texParameteri(gl.TEXTURE_2D,gl.TEXTURE_MIN_FILTER,gl.LINEAR);
  gl.texParameteri(gl.TEXTURE_2D,gl.TEXTURE_MAG_FILTER,gl.LINEAR);
  gl.texParameteri(gl.TEXTURE_2D,gl.TEXTURE_WRAP_S,gl.CLAMP_TO_EDGE);
  gl.texParameteri(gl.TEXTURE_2D,gl.TEXTURE_WRAP_T,gl.CLAMP_TO_EDGE);
  const framebuffer=gl.createFramebuffer();
  let cssWidth=1,cssHeight=1,mapWidth=1,mapHeight=1,dpr=1,frame=0,visible=true;
  const pointer=[0,0],target=[0,0];
  const uniforms=(p,names)=>Object.fromEntries(names.map(name=>[name,gl.getUniformLocation(p,name)]));
  const f=uniforms(formProgram,['uSize','uPointer','uFraming']);
  const s=uniforms(printProgram,['uForm','uCssSize','uPitch','uDpr']);
  function use(p){
    gl.useProgram(p); gl.bindBuffer(gl.ARRAY_BUFFER,quad);
    const location=gl.getAttribLocation(p,'aPosition');
    gl.enableVertexAttribArray(location); gl.vertexAttribPointer(location,2,gl.FLOAT,false,0,0);
  }
  function draw(){
    frame=0;
    if(!visible||document.hidden)return;
    pointer[0]+=(target[0]-pointer[0])*.09; pointer[1]+=(target[1]-pointer[1])*.09;
    gl.bindFramebuffer(gl.FRAMEBUFFER,framebuffer); gl.viewport(0,0,mapWidth,mapHeight);
    use(formProgram); gl.uniform2f(f.uSize,cssWidth,cssHeight); gl.uniform2f(f.uPointer,pointer[0],pointer[1]); gl.uniform1f(f.uFraming,innerWidth<=900?1.43:1.69);
    gl.drawArrays(gl.TRIANGLES,0,6);
    gl.bindFramebuffer(gl.FRAMEBUFFER,null); gl.viewport(0,0,canvas.width,canvas.height);
    use(printProgram); gl.activeTexture(gl.TEXTURE0); gl.bindTexture(gl.TEXTURE_2D,texture);
    gl.uniform1i(s.uForm,0);gl.uniform2f(s.uCssSize,cssWidth,cssHeight);
    gl.uniform1f(s.uPitch,cssWidth<600?2.35:3.4);gl.uniform1f(s.uDpr,dpr);
    gl.drawArrays(gl.TRIANGLES,0,6);
    canvas.dataset.rendered='true';
    if(Math.abs(target[0]-pointer[0])+Math.abs(target[1]-pointer[1])>.002)requestDraw();
  }
  function requestDraw(){if(!frame&&visible&&!document.hidden)frame=requestAnimationFrame(draw);}
  function resize(){
    const box=canvas.getBoundingClientRect(); cssWidth=Math.max(1,box.width);cssHeight=Math.max(1,box.height);
    dpr=Math.min(devicePixelRatio||1,1.75);
    canvas.width=Math.round(cssWidth*dpr);canvas.height=Math.round(cssHeight*dpr);
    const detail=cssWidth<600?.52:.4;
    mapWidth=Math.max(1,Math.round(cssWidth*detail));mapHeight=Math.max(1,Math.round(cssHeight*detail));
    gl.bindTexture(gl.TEXTURE_2D,texture);
    gl.texImage2D(gl.TEXTURE_2D,0,gl.RGBA,mapWidth,mapHeight,0,gl.RGBA,gl.UNSIGNED_BYTE,null);
    gl.bindFramebuffer(gl.FRAMEBUFFER,framebuffer);
    gl.framebufferTexture2D(gl.FRAMEBUFFER,gl.COLOR_ATTACHMENT0,gl.TEXTURE_2D,texture,0);
    gl.bindFramebuffer(gl.FRAMEBUFFER,null); requestDraw();
  }
  new ResizeObserver(resize).observe(canvas);
  new IntersectionObserver(([entry])=>{visible=entry.isIntersecting;if(visible)requestDraw();},{rootMargin:'100px'}).observe(canvas);
  hero.addEventListener('pointermove',event=>{
    if(reducedMotion.matches||event.pointerType==='touch')return;
    const box=hero.getBoundingClientRect();
    target[0]=((event.clientX-box.left)/box.width-.5)*2;
    target[1]=-((event.clientY-box.top)/box.height-.5)*2;
    requestDraw();
  },{passive:true});
  hero.addEventListener('pointerleave',()=>{target[0]=0;target[1]=0;requestDraw();});
  reducedMotion.addEventListener('change',()=>{target[0]=0;target[1]=0;if(reducedMotion.matches){pointer[0]=0;pointer[1]=0;}requestDraw();});
  document.addEventListener('visibilitychange',()=>{if(!document.hidden)requestDraw();});
  canvas.addEventListener('webglcontextlost',event=>{event.preventDefault();cancelAnimationFrame(frame);showFallback();});
  resize();
})();
