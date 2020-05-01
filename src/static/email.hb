<h1>New updates!</h1>

{{#each this}}
	{{#each this}}
		{{#if @first}}
			<h2>
				<a href="{{this.url}}">{{this.name}}</a>
			</h2>
		{{else}}
			{{#each this}}
				<h3>
					<a href="{{this.link}}">{{this.title}}</a>
				</h3>
				<hr>
				<small>
					<a href="{{this.link}}">{{this.link}}</a>
				</small>
				<p>{{{this.description}}}</p>
			{{/each}}
		{{/if}}
	{{/each}}
{{/each}}
